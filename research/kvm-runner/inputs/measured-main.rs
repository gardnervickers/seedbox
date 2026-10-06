use kvm_bindings::*;
use kvm_ioctls::{Kvm,VcpuExit};
use sha2::{Digest,Sha256};
use serde_json::json;
use std::{fs,time::Instant,os::fd::AsRawFd,io::Write};
const MEM:usize=256<<20;
fn hash(b:&[u8])->String {format!("{:x}",Sha256::digest(b))}
fn put(m:&mut[u8],at:usize,v:u64,n:usize){m[at..at+n].copy_from_slice(&v.to_le_bytes()[..n]);}
fn main()->Result<(),Box<dyn std::error::Error>> {
 let args:Vec<_>=std::env::args().collect(); let out=&args[3]; fs::create_dir_all(out)?;
 let kernel=fs::read(&args[1])?;let init=fs::read(&args[2])?;
 let ptr=unsafe{libc::mmap(std::ptr::null_mut(),MEM,3,libc::MAP_PRIVATE|libc::MAP_ANONYMOUS,-1,0)};
 assert_ne!(ptr,libc::MAP_FAILED);let mem=unsafe{std::slice::from_raw_parts_mut(ptr.cast::<u8>(),MEM)};
 let setup=(kernel[0x1f1] as usize+1)*512;mem[0x100000..0x100000+kernel.len()-setup].copy_from_slice(&kernel[setup..]);
 mem[0x101f1..0x10290].copy_from_slice(&kernel[0x1f1..0x290]);
 put(mem,0x10210,0xff,1);put(mem,0x10211,0x81,1);put(mem,0x10218,0x8000000,4);put(mem,0x1021c,init.len() as u64,4);put(mem,0x10228,0x20000,4);
 put(mem,0x101e8,2,1);put(mem,0x102d0,0,8);put(mem,0x102d8,0x9fc00,8);put(mem,0x102e0,1,4);
 put(mem,0x102e4,0x100000,8);put(mem,0x102ec,(MEM-0x100000)as u64,8);put(mem,0x102f4,1,4);
 let cmd=b"console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr noapic nolapic acpi=off tsc=reliable lpj=1000000 panic=-1 random.trust_cpu=off random.trust_bootloader=on vdso=0 clocksource=tsc\0";
 mem[0x20000..0x20000+cmd.len()].copy_from_slice(cmd);mem[0x8000000..0x8000000+init.len()].copy_from_slice(&init);
 put(mem,0x10250,0x30000,8);put(mem,0x30008,9,4);put(mem,0x3000c,32,4);let bootseed=Sha256::digest(b"seedbox-w2-boot-seed-42");mem[0x30010..0x30030].copy_from_slice(&bootseed);let kvm=Kvm::new()?;let vm=kvm.create_vm()?;eprintln!("disable_nx_huge={:?}",vm.enable_cap(&kvm_enable_cap{cap:KVM_CAP_VM_DISABLE_NX_HUGE_PAGES,..Default::default()}));
 unsafe{vm.set_user_memory_region(kvm_userspace_memory_region{slot:0,flags:KVM_MEM_LOG_DIRTY_PAGES,guest_phys_addr:0,memory_size:MEM as u64,userspace_addr:ptr as u64})?;}
 let mut cpu=vm.create_vcpu(0)?;let mut cpuid=kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
 for e in cpuid.as_mut_slice(){if e.function==1 {e.ecx&=!(1<<30);e.ecx&=!(1<<31);e.edx&=!(1<<9);} if e.function==7&&e.index==0{e.ebx&=!(1<<18);} if e.function>=0x40000000&&e.function<0x50000000{e.eax=0;e.ebx=0;e.ecx=0;e.edx=0;}}
 cpu.set_cpuid2(&cpuid)?;let mut s=cpu.get_sregs()?;s.cr0=0x31;s.cr4=0;s.efer=0;
 let seg=kvm_segment{base:0,limit:0xffffffff,selector:0x10,type_:3,present:1,dpl:0,db:1,s:1,l:0,g:1,avl:0,unusable:0,padding:0};
 s.ds=seg;s.es=seg;s.fs=seg;s.gs=seg;s.ss=seg;s.cs=kvm_segment{selector:8,type_:11,..seg};cpu.set_sregs(&s)?;
 cpu.set_regs(&kvm_regs{rip:0x100000,rsi:0x10000,rsp:0x90000,rflags:2,..Default::default()})?;
 let mut serialfile=fs::File::create(format!("{out}/live-serial.txt"))?;let start=Instant::now();let mut exits=0u64;let mut serial=Vec::new();let mut hooks=0u64;let mut rtc=0u8;let mut pit=0u16;let mut p61=0u8;let mut trace=String::new();let mut portcounts=std::collections::BTreeMap::<u16,u64>::new();let mut pending_irq=false;let mut picmask=255u8;let mut vector=0x30u32;let mut icw=0;let mut ticks=0;let mut now=0u64;let mut latched=0u64;let mut clock_reads=0u64;let mut next_tick=4000000u64;let zero_after_arm=args.iter().any(|s|s=="--zero-clock");let cost=1000;let mut armed=false;let mut event_due=u64::MAX;let mut event_fired=false;let mut event_time=0;let mut out_code=None;
 let mut boundary=|cpu:&kvm_ioctls::VcpuFd,tag:u8,seq:u64|->Result<(),Box<dyn std::error::Error>>{let r=cpu.get_regs()?;let regs=[r.rax,r.rbx,r.rcx,r.rdx,r.rsi,r.rdi,r.rsp,r.rbp,r.r8,r.r9,r.r10,r.r11,r.r12,r.r13,r.r14,r.r15,r.rip,r.rflags];trace+=&(json!({"machine_id":0,"incarnation":1,"sequence":seq,"tag":tag,"gpr":regs}).to_string()+"\n");Ok(())};
 loop {
  if exits>3000000||start.elapsed().as_secs()>20 {eprintln!("INFRA_ABORT exits={exits} rip={:x}",cpu.get_regs()?.rip);break;}
  cpu.set_kvm_immediate_exit(0);let mut complete=false;let mut tag=None;let mut done=false;
  match cpu.run(){
   Ok(VcpuExit::IoOut(port,data))=>{*portcounts.entry(port).or_default()+=1;let v=data[0];match port {0x3f8|0xe9=>{serial.push(v);serialfile.write_all(&[v])?;},0x510=>{hooks+=1;tag=Some(v)},0x528=>{armed=true;event_due=now+500000;event_fired=false;},0xf4=>{eprintln!("guest_exit={v}");out_code=Some(v);done=true},0x70=>rtc=v&0x7f,0x61=>p61=v,0x20=>{if v&0x10!=0{icw=1}},0x21=>{if icw==1{vector=v as u32;icw=2}else if icw>0{icw+=1;if icw==4{icw=0}}else{picmask=v}},_=>{}}complete=true;},
   Ok(VcpuExit::IoIn(port,data))=>{*portcounts.entry(port).or_default()+=1;if port==0x520{now+=if armed&&zero_after_arm{0}else{cost};latched=now;clock_reads+=1;}let value=match port {0x3fd=>0x60,0x3f8=>0,0x61=>{p61^=0x20;p61},0x40|0x42=>{pit=pit.wrapping_sub(1000);(pit>>8)as u8},0x71=>match rtc{0xa=>0,0xb=>2,0xc=>0,7|8=>1,9=>0x26,_=>0},0x21=>picmask,0xa1=>255,0x20|0xa0=>0,_=>0xff};if port==0x520{data.copy_from_slice(&(latched as u32).to_le_bytes());}else if port==0x524{data.copy_from_slice(&((latched>>32)as u32).to_le_bytes());tag=Some(b'T');}else{data.fill(value);}complete=true;},
   Ok(VcpuExit::Hlt)=>{now=now.max(next_tick);pending_irq=true;},
   Ok(VcpuExit::IrqWindowOpen)=>{assert_eq!(unsafe {libc::ioctl(cpu.as_raw_fd(),0x4004ae86 as libc::c_ulong,&kvm_interrupt{irq:vector})},0);cpu.get_kvm_run().request_interrupt_window=0;pending_irq=false;ticks+=1;},
   Ok(VcpuExit::MmioRead(_,data))=>{data.fill(0);complete=true;},
   Ok(VcpuExit::MmioWrite(_, _))=>complete=true,
   Err(e) if e.errno()==libc::EINTR=>continue,
   other=>{eprintln!("GUEST_ABORT {other:?}");break;}
  }
  exits+=1;if exits%100000==0{eprintln!("progress exits={exits} hooks={hooks}");}
  if complete{cpu.set_kvm_immediate_exit(1);let c=cpu.run();assert!(matches!(c,Err(e) if e.errno()==libc::EINTR));cpu.set_kvm_immediate_exit(0);}
  if let Some(tag)=tag{boundary(&cpu,tag,hooks+clock_reads)?;}if now>=event_due&&!event_fired{event_fired=true;event_time=now;eprintln!("due_event completed_io=true now_ns={now} clock_reads={clock_reads}");}
  if picmask&1==0&&(now>=next_tick||clock_reads==0&&exits%100==0){pending_irq=true;next_tick=now+4000000;}
  if pending_irq&&picmask&1==0{cpu.get_kvm_run().request_interrupt_window=1;}
  if done{break;}
 }
 fs::write(format!("{out}/serial.txt"),&serial)?;fs::write(format!("{out}/boundaries.jsonl"),&trace)?;
 let dirty=vm.get_dirty_log(0,MEM)?;let dirty_pages:usize=dirty.iter().map(|x|x.count_ones()as usize).sum();
 let threads=fs::read_dir("/proc/self/task")?.map(|x|{let path=x.unwrap().path();let tid=path.file_name().unwrap().to_string_lossy().to_string();let status=fs::read_to_string(path.join("status")).unwrap_or_default();json!({"tid":tid,"status":status})}).collect::<Vec<_>>();
 println!("{}",json!({"boot_seed":42,"boot_seed_sha256":hash(&bootseed),"guest_exit":out_code,"guest_pass":out_code==Some(16),"zero_charge_after_arm":zero_after_arm,"event_deadline_ns":event_due,"clock_read_cost_ns":cost,"virtual_ns":now,"clock_reads":clock_reads,"due_event_fired":event_fired,"due_event_ns":event_time,"kernel_sha256":hash(&kernel),"init_sha256":hash(&init),"ram_sha256":hash(mem),"boundary_sha256":hash(trace.as_bytes()),"serial_sha256":hash(&serial),"exits":exits,"hooks":hooks,"ticks":ticks,"pic_vector":vector,"host_seconds":start.elapsed().as_secs_f64(),"threads":threads,"dirty_pages":dirty_pages,"ports":portcounts}));
 if out_code!=Some(16){std::process::exit(1);}Ok(())
}
