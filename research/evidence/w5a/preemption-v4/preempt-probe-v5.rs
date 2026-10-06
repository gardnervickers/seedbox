#![recursion_limit="256"]
const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/preempt.bin"));
const ORG: u64 = 0x8000;
fn word(b: &[u8], n: usize) -> u64 { u64::from_le_bytes(b[n..n+8].try_into().unwrap()) }
use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_regs, kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use serde_json::{Value, json};
use std::{
    fmt::Display,
    marker::PhantomData,
    rc::Rc,
    thread::{self, ThreadId},
};

type Result<T> = std::result::Result<T, String>;
const SIZE: usize = 0x80000;
const PORT: u16 = 0x505;
fn checked<T, E: Display>(r: std::result::Result<T, E>, context: &str) -> Result<T> {
    r.map_err(|e| format!("{context}: {e}"))
}
fn require(c: bool, message: impl Into<String>) -> Result<()> {
    if c { Ok(()) } else { Err(message.into()) }
}
#[derive(Clone)]
struct Owner {
    thread: ThreadId,
    tid: libc::pid_t,
    _not_send: PhantomData<Rc<()>>,
}
impl Owner {
    fn new() -> Self {
        Self {
            thread: thread::current().id(),
            tid: unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t },
            _not_send: PhantomData,
        }
    }
    fn check(&self) {
        assert_eq!(self.thread, thread::current().id());
        assert_eq!(self.tid, unsafe {
            libc::syscall(libc::SYS_gettid) as libc::pid_t
        });
    }
}
struct Memory {
    pointer: *mut u8,
    owner: Owner,
}
impl Memory {
    fn new(owner: &Owner) -> Result<Self> {
        owner.check();
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        require(
            p != libc::MAP_FAILED,
            format!("mmap: {}", std::io::Error::last_os_error()),
        )?;
        Ok(Self {
            pointer: p.cast(),
            owner: owner.clone(),
        })
    }
    fn bytes(&self) -> &[u8] {
        self.owner.check();
        unsafe { std::slice::from_raw_parts(self.pointer, SIZE) }
    }
    fn write(&mut self, at: usize, bytes: &[u8]) {
        self.owner.check();
        assert!(at + bytes.len() <= SIZE);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.pointer.add(at), bytes.len()) };
    }
    fn put(&mut self, at: usize, v: u64) {
        self.write(at, &v.to_le_bytes());
    }
    fn get(&self, at: usize) -> u64 {
        word(self.bytes(), at)
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        self.owner.check();
        assert_eq!(unsafe { libc::munmap(self.pointer.cast(), SIZE) }, 0);
    }
}
#[derive(Clone, Copy)]
enum Variant { Positive }
struct Machine {
    vcpu: VcpuFd,
    _vm: VmFd,
    memory: Memory,
    owner: Owner,
}
fn segment(selector: u16, code: bool, user: bool) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: u32::MAX,
        selector,
        type_: if code { 11 } else { 3 },
        present: 1,
        dpl: if user { 3 } else { 0 },
        db: if code { 0 } else { 1 },
        s: 1,
        l: u8::from(code),
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}
fn gate(handler: u64, dpl: u8) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..2].copy_from_slice(&(handler as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&8u16.to_le_bytes());
    bytes[5] = 0x8e | (dpl << 5);
    bytes[6..8].copy_from_slice(&((handler >> 16) as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&((handler >> 32) as u32).to_le_bytes());
    bytes
}
impl Machine {
    fn new(kvm: &Kvm, owner: &Owner, variant: Variant) -> Result<Self> {
        owner.check();
        let mut memory = Memory::new(owner)?;
        memory.write(ORG as usize, GUEST);
        memory.put(0x1000, 0x2007);
        memory.put(0x2000, 0x3007);
        memory.put(0x3000, 0x87);
        // Flat code/data plus 64-bit TSS. All pages user-mapped: trusted bare
        // fixture, no kernel/user memory-protection or security claim.
        for (i, v) in [
            0,
            0x00af9b000000ffff,
            0x00cf93000000ffff,
            0x00affb000000ffff,
            0x00cff3000000ffff,
            0x0000890060000067,
            0,
        ]
        .into_iter()
        .enumerate()
        {
            memory.put(0x5000 + i * 8, v);
        }
        memory.put(0x6004, 0x1f000); // unaligned TSS.RSP0
        memory.write(0x6066, &104u16.to_le_bytes()); // no IO bitmap within limit
        memory.write(0x7000 + 13 * 16, &gate(0x9000, 0));
        memory.write(0x7000 + 128 * 16, &gate(0x9000, 3));
        let _ = variant;
        let vm = checked(kvm.create_vm(), "create VM")?;
        unsafe {
            checked(
                vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot: 0,
                    flags: 0,
                    guest_phys_addr: 0,
                    memory_size: SIZE as u64,
                    userspace_addr: memory.pointer as u64,
                }),
                "register memory",
            )?
        };
        let vcpu = checked(vm.create_vcpu(0), "create vCPU")?;
        let cpuid = checked(
            kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES),
            "supported CPUID",
        )?;
        require(
            cpuid
                .as_slice()
                .iter()
                .any(|e| e.function == 0x80000001 && e.edx & (1 << 27) != 0),
            "RDTSCP not supported by host/KVM; probe is inconclusive",
        )?;
        checked(vcpu.set_cpuid2(&cpuid), "set CPUID")?;
        let mut s = checked(vcpu.get_sregs(), "get sregs")?;
        s.cs = segment(8, true, false);
        s.ds = segment(0x10, false, false);
        s.es = s.ds;
        s.ss = s.ds;
        s.fs = s.ds;
        s.gs = s.ds;
        s.gdt.base = 0x5000;
        s.gdt.limit = 55;
        s.idt.base = 0x7000;
        s.idt.limit = 4095;
        s.tr = kvm_segment {
            base: 0x6000,
            limit: 103,
            selector: 0x28,
            type_: 9,
            present: 1,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        s.cr0 = 0x80010033; // PG,WP,NE,ET,MP,PE
        s.cr3 = 0x1000;
        s.cr4 = 1 << 5;
        s.efer = (1 << 8) | (1 << 10); // LME,LMA
        checked(vcpu.set_sregs(&s), "set long-mode sregs")?;
        checked(
            vcpu.set_regs(&kvm_regs {
                rip: ORG + 0x80,
                rsp: 0x1f000,
                rflags: 2,
                ..Default::default()
            }),
            "set regs",
        )?;
        Ok(Self {
            vcpu,
            _vm: vm,
            memory,
            owner: owner.clone(),
        })
    }
}
use kvm_bindings::{kvm_guest_debug,KVM_GUESTDBG_ENABLE,KVM_GUESTDBG_SINGLESTEP};
use std::sync::atomic::{AtomicU64,Ordering};
use sha2::{Digest,Sha256};
static SIGNALS:AtomicU64=AtomicU64::new(0);
static ALARMS:AtomicU64=AtomicU64::new(0);
extern "C" fn signal_handler(sig:i32){if sig==libc::SIGUSR1 {SIGNALS.fetch_add(1,Ordering::Relaxed);} else if sig==libc::SIGALRM {ALARMS.fetch_add(1,Ordering::Relaxed);} }
#[repr(C)]#[derive(Default)]
struct PerfAttr {kind:u32,size:u32,config:u64,period:u64,sample_type:u64,read_format:u64,flags:u64,extra:[u64;9]}
#[repr(C)]struct OwnerEx{kind:i32,pid:i32}
struct Counter{i:i32}
impl Counter {
 fn new(period:u64)->Result<Self>{
  let a=PerfAttr{kind:4,size:128,config:0x5100d1,period,read_format:3,flags:1|(1<<2)|(1<<19),..Default::default()};
  let fd=unsafe{libc::syscall(libc::SYS_perf_event_open,&a,0,-1,-1,0)} as i32;
  require(fd>=0,format!("host guest-only all-CPL perf_event_open: {}",std::io::Error::last_os_error()))?;
  let c=Self{i:fd};
  unsafe {
   let o=OwnerEx{kind:0,pid:libc::syscall(libc::SYS_gettid) as i32};
   require(libc::fcntl(fd,15,&o)==0,"F_SETOWN_EX TID")?;
   require(libc::fcntl(fd,10,libc::SIGUSR1)==0,"F_SETSIG")?;
   require(libc::fcntl(fd,libc::F_SETFL,libc::O_ASYNC|libc::O_NONBLOCK)==0,"F_SETFL async")?;
  }
  Ok(c)
 }
 fn read(&self)->Result<[u64;3]>{let mut r=[0u64;3];require(unsafe{libc::read(self.i,r.as_mut_ptr().cast(),24)}==24,"perf counter read")?;Ok(r)}
 fn enable(&self)->Result<()>{require(unsafe{libc::ioctl(self.i,0x2400,0)}==0,"enable perf")}
 fn period(&self,n:u64)->Result<()>{require(unsafe{libc::ioctl(self.i,0x40082404,&n)}==0,"set perf period")}
}
impl Drop for Counter {fn drop(&mut self){unsafe{libc::ioctl(self.i,0x2401,0);libc::close(self.i);}}}
fn hash(b:&[u8])->String{format!("{:x}",Sha256::digest(b))}
fn regs_value(r:&kvm_regs)->Value {json!({"rax":r.rax,"rbx":r.rbx,"rcx":r.rcx,"rdx":r.rdx,"rsi":r.rsi,"rdi":r.rdi,"rsp":r.rsp,"rbp":r.rbp,"r8":r.r8,"r9":r.r9,"r10":r.r10,"r11":r.r11,"r12":r.r12,"r13":r.r13,"r14":r.r14,"r15":r.r15,"rip":r.rip,"rflags":r.rflags})}
fn completed_io(v:&mut VcpuFd,counter:Option<&Counter>,ram:Option<&[u8]>,debug_allowed:bool,expected_pc:Option<u64>)->Result<Value> {
 let before_regs=regs_value(&v.get_regs().unwrap());
 let before_events=format!("{:?}",v.get_vcpu_events());
 let before_count=counter.map(|c|c.read().unwrap()[0]);
 let before_ram=ram.map(hash);
 v.set_kvm_immediate_exit(1);
 let completion=v.run();
 let was_eintr=matches!(&completion,Err(e) if e.errno()==libc::EINTR);
 let valid_debug=matches!(&completion,Ok(VcpuExit::Debug(d)) if debug_allowed&&d.exception==1&&d.dr6&(1<<14)!=0&&Some(d.pc)==expected_pc);
 let actual_return=format!("{completion:?}");drop(completion);
 v.set_kvm_immediate_exit(0);
 let after=v.get_regs().unwrap();let after_regs=regs_value(&after);
 let after_events=format!("{:?}",v.get_vcpu_events());
 let after_count=counter.map(|c|c.read().unwrap()[0]);let after_ram=ram.map(hash);
 let observation=json!({"requested_immediate_exit":1,"actual_return":actual_return,"before_registers":before_regs,"after_registers":after_regs,"before_events":before_events,"after_events":after_events,"before_count":before_count,"after_count":after_count,"before_ram_sha256":before_ram,"after_ram_sha256":after_ram,"debug_completion":valid_debug});
 require(was_eintr||valid_debug,format!("PIO completion-only return: {observation}"))?;
 require(expected_pc.is_none_or(|pc|after.rip==pc),format!("PIO completion RIP: {observation}"))?;
 require(before_count==after_count&&before_ram==after_ram,format!("PIO completion advanced work/RAM: {observation}"))?;
 Ok(observation)
}
fn terminal(m:&mut Machine,c:&Counter,mode:&str,loops:u64)->Result<Value>{
 let mut exits=0u64;
 loop {
  match m.vcpu.run(){
   Ok(VcpuExit::IoOut(p,d))=>{
    require(p==PORT&&d.len()==4,"continuation IO")?;let tag=u32::from_le_bytes(d.try_into().unwrap());
    completed_io(&mut m.vcpu,Some(c),None,false,None)?;exits+=1;
    if tag==(if mode=="user"{2}else{9}) {break}
    require(mode=="exits"&&tag==3,"unexpected continuation tag")?;
   },Err(e) if e.errno()==libc::EINTR=>continue,
   x=>return Err(format!("continuation exit {x:?}"))
  }
 }
 let r=m.vcpu.get_regs().unwrap();let count=c.read()?;
 require(count[0]==loops,"terminal branch count")?;
 if mode=="rep" {
  require(r.rcx==0&&r.rsi==0x52000&&r.rdi==0x62000,"REP terminal registers")?;
  require(m.memory.bytes()[0x60000..0x62000]==m.memory.bytes()[0x50000..0x52000],"REP terminal bytes")?;
 } else {require(r.r8==7*loops&&r.rcx==0,"loop terminal registers")?;}
 if mode=="exits" {require(m.memory.get(0x4100)==loops,"terminal PIO canary")?;}
 let regs=regs_value(&r);
 Ok(json!({"count":count[0],"registers":regs,"registers_sha256":hash(&serde_json::to_vec(&regs).unwrap()),"ram_sha256":hash(m.memory.bytes()),"debug_registers":format!("{:?}",m.vcpu.get_debug_regs()),"vcpu_events":format!("{:?}",m.vcpu.get_vcpu_events()),"pio_exits_after_landing":exits,"analytic_valid":true,"time_enabled_ns":count[1],"time_running_ns":count[2]}))
}
fn task_inventory()->Result<Value>{
 let mut out=Vec::new();
 for entry in checked(std::fs::read_dir("/proc/self/task"),"task inventory")? {
  let path=checked(entry,"task entry")?.path();
  out.push(json!({"tid":path.file_name().unwrap().to_string_lossy(),"comm":checked(std::fs::read_to_string(path.join("comm")),"task comm")?,"status":checked(std::fs::read_to_string(path.join("status")),"task status")?}));
 }
 Ok(json!(out))
}
fn execute()->Result<Value>{
 let args:Vec<_>=std::env::args().collect();
 let mode=args.get(1).map(String::as_str).unwrap_or("plain");
 let target:u64=args.get(2).map(|s|s.parse().unwrap()).unwrap_or(100000);
 let margin:u64=args.get(3).map(|s|s.parse().unwrap()).unwrap_or(10000);
 let policy=args.get(4).map(String::as_str).unwrap_or("correct");
 let correct=policy=="correct";let reference=policy=="reference";
 require(target>margin&&margin>0,"positive target greater than margin")?;
 unsafe {
  let mut sa:libc::sigaction=std::mem::zeroed();sa.sa_sigaction=signal_handler as *const () as usize;libc::sigemptyset(&mut sa.sa_mask);
  require(libc::sigaction(libc::SIGUSR1,&sa,std::ptr::null_mut())==0,"SIGUSR1 handler")?;
  require(libc::sigaction(libc::SIGALRM,&sa,std::ptr::null_mut())==0,"SIGALRM handler")?;
 }
 let owner=Owner::new();let tasks_before=task_inventory()?;let kvm=checked(Kvm::new(),"KVM")?;let mut m=Machine::new(&kvm,&owner,Variant::Positive)?;
 match m.vcpu.run(){Ok(VcpuExit::IoOut(p,d)) if p==PORT&&d==1u32.to_le_bytes()=>{},x=>return Err(format!("initial PIO {x:?}"))}
 completed_io(&mut m.vcpu,None,None,false,None)?;
 let loops=if mode=="rep" {target} else {target*2}; require(loops<=u32::MAX as u64,"fixture loop length")?;
 m.memory.put(0x4000,loops);
 for n in 0..8192 {m.memory.write(0x50000+n,&[(n as u8).wrapping_mul(7)]);}
 let mut r=m.vcpu.get_regs().unwrap();r.rip=match mode{"plain"=>0xa000,"exits"=>0xb000,"rep"=>0xc000,"uncond"=>0xd000,"user"=>0xe000,_=>return Err("mode".into())};m.vcpu.set_regs(&r).unwrap();
 let initial_registers=regs_value(&r);let initial_ram=hash(m.memory.bytes());
 let expected_rip=match mode{"plain"=>word(GUEST,0),"exits"=>word(GUEST,16),"rep"=>word(GUEST,24),"user"=>word(GUEST,48),_=>0};
 let counter=Counter::new(if reference{1<<60}else{target-margin})?;
 counter.enable()?;
 let t0=std::time::Instant::now();
 let admin_before=counter.read()?[0];let mut x=0u64;
 for n in 0..100000 {x=x.wrapping_add(n);std::hint::black_box(x);}
 let admin_after=counter.read()?[0];require(admin_before==admin_after,"host work leaked into guest perf counter")?;
 if reference {return Ok(json!({"mode":mode,"target":target,"margin":margin,"reference":true,"single_thread_tid":owner.tid,"tasks_before":tasks_before,"tasks_after":task_inventory()?,"caller_threads":std::fs::read_dir("/proc/self/task").unwrap().count(),"perf_config":0x5100d1u64,"perf_flags":1|(1<<2)|(1<<19),"initial_registers":initial_registers,"initial_ram_sha256":initial_ram,"terminal":terminal(&mut m,&counter,mode,loops)?}));}
 if mode=="uncond" {unsafe{libc::alarm(1);}}
 let mut exits=0u64;let mut eintr=0u64;let mut boundaries=Vec::new();
 loop {
  owner.check();m.owner.check();
  match m.vcpu.run(){
   Err(e) if e.errno()==libc::EINTR=>{eintr+=1;if SIGNALS.load(Ordering::Relaxed)>0||ALARMS.load(Ordering::Relaxed)>0 {break}},
   Ok(VcpuExit::IoOut(p,d))=>{require(p==PORT&&d.len()==4,"IO")?;let tag=u32::from_le_bytes(d.try_into().unwrap());completed_io(&mut m.vcpu,Some(&counter),None,false,None)?;exits+=1;boundaries.push(json!({"tag":tag,"count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));if SIGNALS.load(Ordering::Relaxed)>0 {break} require(tag!=9,"guest completed before overflow signal")?;},
   v=>return Err(format!("run to overflow: {v:?}"))
  }
 }
 let overflow=counter.read()?;let overflow_regs=m.vcpu.get_regs().unwrap();
 let mut steps=0u64;let mut debug_exits=0u64;let mut debug_records=Vec::new();let mut overshoot=false;let mut landed=false;let mut pio_witness:Option<Value>=None;let mut canary_next:Option<u64>=None;
 counter.period(1<<60)?;
 if correct&&mode!="uncond" {
  let dbg=kvm_guest_debug{control:KVM_GUESTDBG_ENABLE|KVM_GUESTDBG_SINGLESTEP,..Default::default()};m.vcpu.set_guest_debug(&dbg).map_err(|e|format!("KVM guest debug enable: {e}"))?;
  loop {
   let count=counter.read()?[0];let r=m.vcpu.get_regs().unwrap();
   if count>target || (mode=="rep"&&count==target&&r.rip==expected_rip&&(r.rcx<4096||r.rsi>0x51000||r.rdi>0x61000)) {overshoot=true;break}
   if count==target&&r.rip==expected_rip&&(mode!="rep"||r.rcx==4096&&r.rsi==0x51000&&r.rdi==0x61000){landed=true;break}
   if steps>200000 {break}
   m.vcpu.set_guest_debug(&dbg).map_err(|e|format!("KVM guest debug step arm: {e}"))?;
   match m.vcpu.run(){
    Ok(VcpuExit::Debug(d))=>{debug_exits+=1;if debug_records.len()<20 {debug_records.push(json!({"exception":d.exception,"pc":d.pc,"dr6":d.dr6,"dr7":d.dr7,"count":counter.read()?[0]}));}},
    Ok(VcpuExit::IoOut(p,d))=>{require(p==PORT&&d.len()==4,"step IO")?;let tag=u32::from_le_bytes(d.try_into().unwrap());let canary_before=m.memory.get(0x4100);
     let observe=mode=="exits"&&pio_witness.is_none();
     let completion=completed_io(&mut m.vcpu,Some(&counter),if observe{Some(m.memory.bytes())}else{None},true,if mode=="exits"&&tag==3{Some(word(GUEST,56))}else{None})?;
     require(m.memory.get(0x4100)==canary_before,"completion executed following canary")?;
     if observe {pio_witness=Some(json!({"completion":completion,"canary_before":canary_before,"canary_after":m.memory.get(0x4100)}));canary_next=Some(canary_before+1);}
     exits+=1;boundaries.push(json!({"tag":tag,"phase":"correction","count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));require(tag!=9,"guest completed during correction")?;},
    Err(e) if e.errno()==libc::EINTR=>{eintr+=1;continue},
    v=>return Err(format!("single step: {v:?}"))
   }
   if let Some(expected)=canary_next {
    let next=m.vcpu.get_regs().unwrap();
    if next.rip==word(GUEST,64) {
     require(m.memory.get(0x4100)==expected,"following instruction canary")?;
     if let Some(w)=pio_witness.as_mut(){w["following_instruction"]=json!({"rip":next.rip,"canary":expected,"count":counter.read()?[0]});}
     canary_next=None;
    }
   }
   steps+=1;
  }
  m.vcpu.set_guest_debug(&kvm_guest_debug::default()).unwrap();
 }
 let final_read=counter.read()?;let r=m.vcpu.get_regs().unwrap();let regs=regs_value(&r);let reg_bytes=serde_json::to_vec(&regs).unwrap();
 let landing_ram=hash(m.memory.bytes());
 let elapsed=t0.elapsed().as_nanos();
 let debug_registers=format!("{:?}",m.vcpu.get_debug_regs());let vcpu_events=format!("{:?}",m.vcpu.get_vcpu_events());
 let rep_bytes_valid=mode!="rep" || (m.memory.bytes()[0x60000..0x61000]==m.memory.bytes()[0x50000..0x51000]&&m.memory.bytes()[0x61000..0x62000].iter().all(|b|*b==0));
 let left=(loops-target) as u32;
 let expected_flags=2u64 | if (left as u8).count_ones()%2==0{4}else{0} | if left&15==15{16}else{0} | if left==0{64}else{0} | if left&(1<<31)!=0{128}else{0} | if left==0x7fffffff{2048}else{0};
 let analytic_valid=if mode=="rep" {r.rcx==4096&&r.rsi==0x51000&&r.rdi==0x61000&&r.rflags==70&&rep_bytes_valid} else {r.r8==7*target&&r.rcx==loops-target&&r.rflags==expected_flags&&(mode!="exits"||m.memory.get(0x4100)==target)};
 let perf_valid=final_read[1]>0&&final_read[1]==final_read[2];
 let mut accepted=!correct || (landed&&!overshoot&&final_read[0]==target&&r.rip==expected_rip&&analytic_valid&&perf_valid&&(mode!="exits"||(pio_witness.is_some()&&canary_next.is_none())));
 let mut terminal_error:Option<String>=None;
 let continued=if correct&&accepted {match terminal(&mut m,&counter,mode,loops){Ok(t)=>Some(t),Err(e)=>{terminal_error=Some(e);accepted=false;None}}}else{None};
 let threads=std::fs::read_dir("/proc/self/task").unwrap().count();
 Ok(json!({"mode":mode,"target":target,"margin":margin,"correction":correct,"accepted":accepted,"analytic_valid":analytic_valid,"rep_bytes_valid":rep_bytes_valid,"perf_valid":perf_valid,"terminal":continued,"terminal_error":terminal_error,"debug_registers":debug_registers,"vcpu_events":vcpu_events,"pio_witness":pio_witness,"tasks_before":tasks_before,"tasks_after":task_inventory()?,"caller_threads":threads,"landed":landed,"overshoot":overshoot,"expected_rip":expected_rip,"initial_registers":initial_registers,"initial_ram_sha256":initial_ram,"overflow_count":overflow[0],"overflow_registers":regs_value(&overflow_regs),"final_count":final_read[0],"final_registers":regs,"registers_sha256":hash(&reg_bytes),"ram_sha256":landing_ram,"correction_steps":steps,"debug_exits":debug_exits,"debug_records":debug_records,"pio_boundaries":boundaries,"vm_io_exits":exits,"eintr":eintr,"signals":SIGNALS.load(Ordering::Relaxed),"watchdog_alarms":ALARMS.load(Ordering::Relaxed),"host_admin_count_before":admin_before,"host_admin_count_after":admin_after,"time_enabled_ns":final_read[1],"time_running_ns":final_read[2],"elapsed_ns":elapsed,"perf_config":0x5100d1u64,"perf_flags":1|(1<<2)|(1<<19),"single_thread_tid":owner.tid}))
}
fn main(){match execute(){Ok(v)=>{println!("{}",v);if v.get("accepted")==Some(&json!(false)){std::process::exit(2)}},Err(e)=>{eprintln!("{}",json!({"error":e}));std::process::exit(1)}}}
