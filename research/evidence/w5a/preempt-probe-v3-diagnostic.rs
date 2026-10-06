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
fn completed_io(v:&mut VcpuFd)->Result<()> {
 v.set_kvm_immediate_exit(1);
 let completion=v.run();
 require(matches!(&completion,Err(e) if e.errno()==libc::EINTR),format!("PIO completion-only reentry: {completion:?}"))?;
 v.set_kvm_immediate_exit(0);Ok(())
}
fn execute()->Result<Value>{
 let args:Vec<_>=std::env::args().collect();
 let mode=args.get(1).map(String::as_str).unwrap_or("plain");
 let target:u64=args.get(2).map(|s|s.parse().unwrap()).unwrap_or(100000);
 let margin:u64=args.get(3).map(|s|s.parse().unwrap()).unwrap_or(10000);
 let correct=args.get(4).map(String::as_str).unwrap_or("correct")=="correct";
 require(target>margin&&margin>0,"positive target greater than margin")?;
 unsafe {
  let mut sa:libc::sigaction=std::mem::zeroed();sa.sa_sigaction=signal_handler as *const () as usize;libc::sigemptyset(&mut sa.sa_mask);
  require(libc::sigaction(libc::SIGUSR1,&sa,std::ptr::null_mut())==0,"SIGUSR1 handler")?;
  require(libc::sigaction(libc::SIGALRM,&sa,std::ptr::null_mut())==0,"SIGALRM handler")?;
 }
 let owner=Owner::new();let kvm=checked(Kvm::new(),"KVM")?;let mut m=Machine::new(&kvm,&owner,Variant::Positive)?;
 match m.vcpu.run(){Ok(VcpuExit::IoOut(p,d)) if p==PORT&&d==1u32.to_le_bytes()=>{},x=>return Err(format!("initial PIO {x:?}"))}
 completed_io(&mut m.vcpu)?;
 let loops=if mode=="rep" {target} else {target*2}; require(loops<=u32::MAX as u64,"fixture loop length")?;
 m.memory.put(0x4000,loops);
 for n in 0..8192 {m.memory.write(0x50000+n,&[(n as u8).wrapping_mul(7)]);}
 let mut r=m.vcpu.get_regs().unwrap();r.rip=match mode{"plain"=>0xa000,"exits"=>0xb000,"rep"=>0xc000,"uncond"=>0xd000,"user"=>0xe000,_=>return Err("mode".into())};m.vcpu.set_regs(&r).unwrap();
 let initial_registers=regs_value(&r);let initial_ram=hash(m.memory.bytes());
 let expected_rip=match mode{"plain"=>word(GUEST,0),"exits"=>word(GUEST,16),"rep"=>word(GUEST,24),"user"=>word(GUEST,48),_=>0};
 let counter=Counter::new(target-margin)?;
 counter.enable()?;
 let t0=std::time::Instant::now();
 let admin_before=counter.read()?[0];let mut x=0u64;
 for n in 0..100000 {x=x.wrapping_add(n);std::hint::black_box(x);}
 let admin_after=counter.read()?[0];require(admin_before==admin_after,"host work leaked into guest perf counter")?;
 if mode=="uncond" {unsafe{libc::alarm(1);}}
 let mut exits=0u64;let mut eintr=0u64;let mut boundaries=Vec::new();
 loop {
  owner.check();m.owner.check();
  match m.vcpu.run(){
   Err(e) if e.errno()==libc::EINTR=>{eintr+=1;if SIGNALS.load(Ordering::Relaxed)>0||ALARMS.load(Ordering::Relaxed)>0 {break}},
   Ok(VcpuExit::IoOut(p,d))=>{require(p==PORT&&d.len()==4,"IO")?;let tag=u32::from_le_bytes(d.try_into().unwrap());completed_io(&mut m.vcpu)?;exits+=1;boundaries.push(json!({"tag":tag,"count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));if SIGNALS.load(Ordering::Relaxed)>0 {break} require(tag!=9,"guest completed before overflow signal")?;},
   v=>return Err(format!("run to overflow: {v:?}"))
  }
 }
 let overflow=counter.read()?;let overflow_regs=m.vcpu.get_regs().unwrap();
 let mut steps=0u64;let mut debug_exits=0u64;let mut debug_records=Vec::new();let mut overshoot=false;let mut landed=false;
 counter.period(1<<60)?;
 if correct&&mode!="uncond" {
  let dbg=kvm_guest_debug{control:KVM_GUESTDBG_ENABLE|KVM_GUESTDBG_SINGLESTEP,..Default::default()};m.vcpu.set_guest_debug(&dbg).map_err(|e|format!("KVM guest debug enable: {e}"))?;
  loop {
   let count=counter.read()?[0];let r=m.vcpu.get_regs().unwrap();
   if count>target {overshoot=true;break}
   if count==target&&r.rip==expected_rip&&(mode!="rep"||r.rcx==4096){landed=true;break}
   if steps>200000 {break}
   m.vcpu.set_guest_debug(&dbg).map_err(|e|format!("KVM guest debug step arm: {e}"))?;
   match m.vcpu.run(){
    Ok(VcpuExit::Debug(d))=>{debug_exits+=1;if debug_records.len()<20 {debug_records.push(json!({"exception":d.exception,"pc":d.pc,"dr6":d.dr6,"dr7":d.dr7,"count":counter.read()?[0]}));}},
    Ok(VcpuExit::IoOut(p,d))=>{require(p==PORT&&d.len()==4,"step IO")?;let tag=u32::from_le_bytes(d.try_into().unwrap());completed_io(&mut m.vcpu)?;exits+=1;boundaries.push(json!({"tag":tag,"count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));require(tag!=9,"guest completed during correction")?;},
    Err(e) if e.errno()==libc::EINTR=>{eintr+=1;continue},
    v=>return Err(format!("single step: {v:?}"))
   }
   steps+=1;
  }
  m.vcpu.set_guest_debug(&kvm_guest_debug::default()).unwrap();
 }
 let final_read=counter.read()?;let r=m.vcpu.get_regs().unwrap();let regs=regs_value(&r);let reg_bytes=serde_json::to_vec(&regs).unwrap();
 let elapsed=t0.elapsed().as_nanos();
 Ok(json!({"mode":mode,"target":target,"margin":margin,"correction":correct,"landed":landed,"overshoot":overshoot,"expected_rip":expected_rip,"initial_registers":initial_registers,"initial_ram_sha256":initial_ram,"overflow_count":overflow[0],"overflow_registers":regs_value(&overflow_regs),"final_count":final_read[0],"final_registers":regs,"registers_sha256":hash(&reg_bytes),"ram_sha256":hash(m.memory.bytes()),"correction_steps":steps,"debug_exits":debug_exits,"debug_records":debug_records,"pio_boundaries":boundaries,"vm_io_exits":exits,"eintr":eintr,"signals":SIGNALS.load(Ordering::Relaxed),"watchdog_alarms":ALARMS.load(Ordering::Relaxed),"host_admin_count_before":admin_before,"host_admin_count_after":admin_after,"time_enabled_ns":final_read[1],"time_running_ns":final_read[2],"elapsed_ns":elapsed,"perf_config":0x5100d1u64,"perf_flags":1|(1<<2)|(1<<19),"single_thread_tid":owner.tid}))
}
fn main(){match execute(){Ok(v)=>println!("{}",v),Err(e)=>{eprintln!("{}",json!({"error":e}));std::process::exit(1)}}}
