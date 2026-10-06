const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.bin"));
const ORG: u64 = 0x8000;
fn word(b: &[u8], n: usize) -> u64 { u64::from_le_bytes(b[n..n+8].try_into().unwrap()) }
use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_regs, kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd, VmFd};
use serde_json::{Value, json};
use std::{
    fmt::Display,
    marker::PhantomData,
    rc::Rc,
    thread::{self, ThreadId},
};

type Result<T> = std::result::Result<T, String>;
const SIZE: usize = 0x40000;
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
use kvm_bindings::{Msrs,kvm_msr_entry};
const CTL:u32=0xc0010000;
const CTR:u32=0xc0010004;
const MASK:u64=(1u64<<48)-1;
fn msr_set(v:&VcpuFd,index:u32,data:u64)->Result<()> {
 let m=Msrs::from_entries(&[kvm_msr_entry{index,data,..Default::default()}]).unwrap();
 let n=checked(v.set_msrs(&m),"set MSR")?; require(n==1,format!("MSR {index:x} unsupported, wrote {n}"))
}
fn msr_get(v:&VcpuFd,index:u32)->Result<u64> {
 let mut m=Msrs::from_entries(&[kvm_msr_entry{index,..Default::default()}]).unwrap();
 let n=checked(v.get_msrs(&mut m),"get MSR")?;require(n==1,"MSR get short")?;Ok(m.as_slice()[0].data)
}
fn boundary(v:&mut VcpuFd)->Result<(u32,u64)> {
 loop {match v.run() {
 Ok(VcpuExit::IoOut(port,data))=>{
  require(port==PORT&&data.len()==4,"unexpected IO")?;
  let tag=u32::from_le_bytes(data.try_into().unwrap());
  v.set_kvm_immediate_exit(1);
  require(matches!(v.run(),Err(e) if e.errno()==libc::EINTR),"completion-only reentry must EINTR")?;
  v.set_kvm_immediate_exit(0);
  return Ok((tag,checked(v.get_regs(),"regs")?.rip));
 },Err(e) if e.errno()==libc::EINTR=>continue,
 x=>return Err(format!("unexpected KVM exit {x:?}"))}}
}
#[repr(C)]
#[derive(Default)]
struct PerfAttr {kind:u32,size:u32,config:u64,period:u64,sample_type:u64,read_format:u64,flags:u64,extra:[u64;9]}
fn perf_probe(config:u64,flags:u64)->Value {
 let a=PerfAttr{kind:4,size:128,config,flags:flags|1,..Default::default()};
 let fd=unsafe{libc::syscall(libc::SYS_perf_event_open,&a,0,-1,-1,0)};
 if fd<0 {json!({"config":config,"flags":a.flags,"errno":std::io::Error::last_os_error().raw_os_error()})}
 else {unsafe{libc::close(fd as i32);};json!({"config":config,"flags":a.flags,"open":true})}
}
fn native_lock_probe()->Value {
 let a=PerfAttr{kind:4,size:128,config:0x825,flags:1|(1<<5)|(1<<6)|(1<<20),..Default::default()};
 let fd=unsafe{libc::syscall(libc::SYS_perf_event_open,&a,0,-1,-1,0)} as i32;
 if fd<0 {return json!({"errno":std::io::Error::last_os_error().raw_os_error()});}
 let mut x:u32=0; let mut count:u64=0;
 unsafe {libc::ioctl(fd,0x2400,0);std::arch::asm!("lock add dword ptr [{p}], 1",p=in(reg)&mut x,options(nostack));libc::ioctl(fd,0x2401,0);libc::read(fd,(&mut count as *mut u64).cast(),8);libc::close(fd);}
 json!({"config":0x825,"one_lock_add_result":x,"count":count})
}
fn execute()->Result<Value> {
 let args:Vec<_>=std::env::args().collect();
 let mode=args.get(1).map(String::as_str).unwrap_or("branch");
 let iterations:u32=args.get(2).map(|s|s.parse().unwrap()).unwrap_or(10000);
 let cpl=args.get(3).map(String::as_str).unwrap_or("both");
 let initial:u64=args.get(4).map(|s|s.parse().unwrap()).unwrap_or(0);
 let event:u64=args.get(5).map(|s|s.parse().unwrap()).unwrap_or(0xd1);
 let admin:usize=args.get(6).map(|s|s.parse().unwrap()).unwrap_or(0);
 let owner=Owner::new(); let kvm=checked(Kvm::new(),"KVM")?;
 let mut m=Machine::new(&kvm,&owner,Variant::Positive)?;
 let cpuid=kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap();
 let pmu:Vec<_>=cpuid.as_slice().iter().filter(|e|[0xa,0x80000001,0x80000022].contains(&e.function)).map(|e|json!({"leaf":e.function,"eax":e.eax,"ebx":e.ebx,"ecx":e.ecx,"edx":e.edx})).collect();
 boundary(&mut m.vcpu)?;
 let ctl=event | (1<<22) | match cpl {"user"=>1<<16,"kernel"=>1<<17,_=>(1<<16)|(1<<17)};
 msr_set(&m.vcpu,CTL,0)?;msr_set(&m.vcpu,CTR,initial)?;msr_set(&m.vcpu,CTL,ctl)?;
 let configured_control=msr_get(&m.vcpu,CTL)?;
 let before=msr_get(&m.vcpu,CTR)?;
 let mut accum=0u64;
 for n in 0..admin {accum=accum.wrapping_add(n as u64);std::hint::black_box(accum);}
 let after_admin=msr_get(&m.vcpu,CTR)?;
 m.memory.put(0x4000,iterations as u64);
 let mut regs=checked(m.vcpu.get_regs(),"regs")?;
 regs.rip=match mode {"branch"=>0xa000,"extra"=>0xb000,"nonbranch"=>0xc000,"user"=>0x38000,"lock"=>0x39000,_=>return Err("mode".into())};
 checked(m.vcpu.set_regs(&regs),"set fixture RIP")?;
 let (tag,rip)=boundary(&mut m.vcpu)?;
 let end=msr_get(&m.vcpu,CTR)?;
 msr_set(&m.vcpu,CTL,0)?;
 let ctl_read=msr_get(&m.vcpu,CTL)?;
 Ok(json!({"mode":mode,"iterations":iterations,"cpl_filter":cpl,"event":event,"control":ctl,"configured_control":configured_control,"initial":initial,"before":before,"after_host_admin":after_admin,"host_admin_iterations":admin,"end":end,"delta_48":end.wrapping_sub(before)&MASK,"boundary_tag":tag,"completed_rip":rip,"guest_result":m.memory.get(0x4020),"disabled_control":ctl_read,"cpuid":pmu,"native_speclockmap_probe":native_lock_probe(),"perf_guest_all":perf_probe(0xd1,1<<19),"perf_guest_user":perf_probe(0xd1,(1<<19)|(1<<5)),"perf_host_user":perf_probe(0xd1,(1<<20)|(1<<5))}))
}
fn main(){ match execute(){Ok(v)=>println!("{}",v),Err(e)=>{eprintln!("{}",json!({"error":e}));std::process::exit(1)}} }
