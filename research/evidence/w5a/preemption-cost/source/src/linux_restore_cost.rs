use super::*;
use kvm_bindings::*;
use std::{fs, path::Path};
const INPUT:&str="/home/gvickers/seedbox-research/w5a-round-1/evidence/w5a/linux-start-input-v2";
const LMEM:usize=256<<20;
fn raw<T>(name:&str)->Result<T>{
 let b=checked(fs::read(Path::new(INPUT).join(format!("{name}.bin"))),"read CPU input")?;
 require(b.len()==std::mem::size_of::<T>(),format!("CPU input ABI size {name}"))?;
 Ok(unsafe{std::ptr::read_unaligned(b.as_ptr().cast::<T>())})
}
fn state_hash<T>(v:&T)->String{hash(unsafe{std::slice::from_raw_parts((v as *const T).cast::<u8>(),std::mem::size_of::<T>())})}
fn restore(kvm:&Kvm,owner:&Owner)->Result<(Machine,Value)> {
 let manifest_bytes=checked(fs::read(Path::new(INPUT).join("input-manifest.json")),"input manifest")?;
 require(hash(&manifest_bytes)=="571b39767430b5ecaba9bbcbabfdb1b1ea89600bb13003cb4b8444706f065f8c","pinned manifest hash")?;
 let manifest:Value=checked(serde_json::from_slice(&manifest_bytes),"manifest JSON")?;
 for (name,want) in manifest.as_object().ok_or("input manifest object")? {
  require(!name.contains('/')&&!name.contains(".."),"invalid input member name")?;
  let b=checked(fs::read(Path::new(INPUT).join(name)),&format!("input member {name}"))?;
  require(Some(hash(&b).as_str())==want.as_str(),format!("input hash mismatch {name}"))?;
 }
 let profile:Value=checked(serde_json::from_slice(&checked(fs::read(Path::new(INPUT).join("profile.json")),"profile")?),"profile JSON")?;
 require(profile["world"]["pending_irq"]==false,"pinned interval requires no pending world IRQ")?;
 require(profile["world"]["init_sha256"]=="51df3c52c8d797e97898c90d6995a1673da0fa631d36ff9cc85d1d11a49e4fa7","two-phase fixture input")?;
 let mut memory=Memory::new_sized(owner,LMEM)?;
 let ram=checked(fs::read(Path::new(INPUT).join("ram.bin")),"RAM input")?;require(ram.len()==LMEM,"RAM size")?;memory.write(0,&ram);drop(ram);
 let vm=checked(kvm.create_vm(),"Linux VM")?;
 // Same profile as capture: no in-kernel irqchip/PIT, and no new timer/device
 // IRQ injections during this bounded user loop. NX helper is observed below.
 unsafe{checked(vm.set_user_memory_region(kvm_userspace_memory_region{slot:0,flags:KVM_MEM_LOG_DIRTY_PAGES,guest_phys_addr:0,memory_size:LMEM as u64,userspace_addr:memory.pointer as u64}),"Linux memory")?;}
 require(kvm.check_extension_int(kvm_ioctls::Cap::Xsave2)<=4096,"XSAVE2 host area exceeds pinned 4096-byte input")?;
 let vcpu=checked(vm.create_vcpu(0),"Linux vCPU")?;
 let cp:Value=checked(serde_json::from_slice(&checked(fs::read(Path::new(INPUT).join("guest-cpuid.json")),"CPUID input")?),"CPUID JSON")?;
 let entries=cp.as_array().ok_or("CPUID array")?.iter().map(|e|kvm_cpuid_entry2{function:e["function"].as_u64().unwrap() as u32,index:e["index"].as_u64().unwrap() as u32,flags:e["flags"].as_u64().unwrap() as u32,eax:e["eax"].as_u64().unwrap() as u32,ebx:e["ebx"].as_u64().unwrap() as u32,ecx:e["ecx"].as_u64().unwrap() as u32,edx:e["edx"].as_u64().unwrap() as u32,..Default::default()}).collect::<Vec<_>>();
 checked(vcpu.set_cpuid2(&checked(CpuId::from_entries(&entries),"CPUID entries")?),"restore CPUID")?;
 let s:kvm_sregs=raw("sregs")?;checked(vcpu.set_sregs(&s),"restore sregs")?;
 let ms:Value=checked(serde_json::from_slice(&checked(fs::read(Path::new(INPUT).join("msrs.json")),"MSR input")?),"MSR JSON")?;
 let mut readonly=Vec::new();
 for e in ms.as_array().ok_or("MSR array")? {
  let index=e["index"].as_u64().unwrap() as u32;let data=e["data"].as_u64().unwrap();
  let one=checked(Msrs::from_entries(&[kvm_msr_entry{index,data,..Default::default()}]),"MSR entry")?;
  if !matches!(vcpu.set_msrs(&one),Ok(1)) {
   let mut current=checked(Msrs::from_entries(&[kvm_msr_entry{index,..Default::default()}]),"MSR readback entry")?;
   require(matches!(vcpu.get_msrs(&mut current),Ok(1))&&current.as_slice()[0].data==data,format!("cannot restore MSR {index:x}"))?;readonly.push(index);
  }
 }
 checked(vcpu.set_xcrs(&raw("xcrs")?),"restore XCRS")?;
 checked(vcpu.set_fpu(&raw("fpu")?),"restore FPU")?;
 // Fixed 4096-byte KVM_GET_XSAVE input captured on this same host/CPUID ABI.
 unsafe{checked(vcpu.set_xsave(&raw("xsave")?),"restore XSAVE")?;}
 checked(vcpu.set_debug_regs(&raw("debug_regs")?),"restore debug registers")?;
 checked(vcpu.set_mp_state(raw("mp_state")?),"restore MP state")?;
 checked(vcpu.set_vcpu_events(&raw("vcpu_events")?),"restore vCPU events")?;
 let r:kvm_regs=raw("regs")?;checked(vcpu.set_regs(&r),"restore registers")?;
 let got=checked(vcpu.get_regs(),"read restored GPR")?;
 require(regs_value(&got)==regs_value(&r),"restored initial GPR differs")?;
 require(state_hash(&vcpu.get_sregs().unwrap())==state_hash(&s),"restored sregs differs")?;
 let initial_state=json!({"manifest_sha256":hash(&manifest_bytes),"profile":profile,"readonly_equal_msrs":readonly,"sregs_sha256":state_hash(&vcpu.get_sregs().unwrap()),"fpu_sha256":state_hash(&vcpu.get_fpu().unwrap()),"xsave_sha256":state_hash(&vcpu.get_xsave().unwrap()),"xcrs_sha256":state_hash(&vcpu.get_xcrs().unwrap()),"debug_registers":format!("{:?}",vcpu.get_debug_regs()),"vcpu_events":format!("{:?}",vcpu.get_vcpu_events()),"mp_state":format!("{:?}",vcpu.get_mp_state())});
 Ok((Machine{vcpu,_vm:vm,memory,owner:owner.clone()},initial_state))
}
fn io(v:&mut VcpuFd,c:&Counter,debug:bool)->Result<Value>{completed_io(v,Some(c),None,debug,if debug{Some(0x4010a8)}else{None})}
fn finish(m:&mut Machine,c:&Counter)->Result<Value>{
 let mut boundaries=Vec::new();loop{m.owner.check();match m.vcpu.run(){
  Ok(VcpuExit::IoOut(0x530,d))=>{require(d.len()==1,"Linux PIO size")?;let tag=d[0];io(&mut m.vcpu,c,false)?;boundaries.push(json!({"tag":tag,"count":c.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));if tag==2 {break}require(tag==3,"Linux continuation tag")?;},
  Err(e) if e.errno()==libc::EINTR=>continue,x=>return Err(format!("Linux continuation {x:?}"))
 }}
 let r=m.vcpu.get_regs().unwrap();let read=c.read()?;require(read[0]==2_000_000&&r.rcx==0&&r.rax==2&&r.rdi==1_000_000&&r.rsi==1_000_000&&r.rip==0x4010c3,"Linux terminal analytic state")?;
 let rv=regs_value(&r);Ok(json!({"count":read[0],"registers":rv,"registers_sha256":hash(&serde_json::to_vec(&rv).unwrap()),"ram_sha256":hash(m.memory.bytes()),"debug_registers":format!("{:?}",m.vcpu.get_debug_regs()),"vcpu_events":format!("{:?}",m.vcpu.get_vcpu_events()),"time_enabled_ns":read[1],"time_running_ns":read[2],"pio_boundaries":boundaries}))
}
pub(super) fn execute(kvm:&Kvm,owner:&Owner,target:u64,margin:u64,policy:&str,tasks_before:Value)->Result<Value>{
 let setup_start=std::time::Instant::now();
 require(target==100_000||target==1_000_100||target==1_100_000,"declared Linux target")?;
 let breakpoint=policy=="breakpoint";let count_only=policy=="count_only";let correct=policy=="correct"||breakpoint||count_only;let reference=policy=="reference";
 let(mut m,input_state)=restore(kvm,owner)?;let restore_ns=setup_start.elapsed().as_nanos();let initial_witness_start=std::time::Instant::now();let initial_registers=regs_value(&m.vcpu.get_regs().unwrap());let initial_ram=hash(m.memory.bytes());let initial_witness_ns=initial_witness_start.elapsed().as_nanos();
 let counter=Counter::new(if reference{1<<60}else{target-margin})?;counter.enable()?;
 let admin_before=counter.read()?[0];for n in 0..100000{std::hint::black_box(n);}let admin_after=counter.read()?[0];require(admin_before==admin_after,"Linux host counter exclusion")?;
 let setup_ns=setup_start.elapsed().as_nanos();
 if reference{return Ok(json!({"mode":"linux","target":target,"margin":margin,"reference":true,"single_thread_tid":owner.tid,"tasks_before":tasks_before,"tasks_after":task_inventory()?,"perf_config":0x5100d1u64,"perf_flags":524293,"initial_registers":initial_registers,"initial_ram_sha256":initial_ram,"input_state":input_state,"terminal":finish(&mut m,&counter)?}));}
 let start=std::time::Instant::now();let mut boundaries=Vec::new();let mut eintr=0u64;
 loop{owner.check();m.owner.check();match m.vcpu.run(){
  Err(e) if e.errno()==libc::EINTR=>{eintr+=1;if SIGNALS.load(Ordering::Relaxed)>0{break}},
  Ok(VcpuExit::IoOut(0x530,d))=>{require(d.len()==1,"Linux PIO size")?;let tag=d[0];io(&mut m.vcpu,&counter,false)?;boundaries.push(json!({"tag":tag,"phase":"overflow","count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip}));require(tag==3,"Linux terminal before overflow")?;if SIGNALS.load(Ordering::Relaxed)>0{break}},
  x=>return Err(format!("Linux overflow {x:?}"))
 }}
 let overflow_elapsed=start.elapsed().as_nanos();let correction_start=std::time::Instant::now();
 let overflow=counter.read()?;let overflow_regs=regs_value(&m.vcpu.get_regs().unwrap());counter.period(1<<60)?;
 let expected_rip=if target<1_000_000{0x401097}else{0x4010b2};let progress=target%1_000_000;
 let mut steps=0u64;let mut overshoot=false;let mut landed=false;let mut debug_exits=0u64;
 if correct{
  let mut dbg=kvm_guest_debug{control:KVM_GUESTDBG_ENABLE|KVM_GUESTDBG_SINGLESTEP,..Default::default()};
  if breakpoint{dbg.control=KVM_GUESTDBG_ENABLE|kvm_bindings::KVM_GUESTDBG_USE_HW_BP;dbg.arch.debugreg[0]=expected_rip;dbg.arch.debugreg[7]=0x401;}
  loop{owner.check();m.owner.check();let count=counter.read()?[0];let r=m.vcpu.get_regs().unwrap();
   if count>target||(count_only&&steps==0&&count==target){overshoot=true;break}if count==target&&(count_only||r.rip==expected_rip){landed=true;break}if steps>200000{break}
   if breakpoint&&r.rip==expected_rip{let mut resumed=r;resumed.rflags|=1<<16;m.vcpu.set_regs(&resumed).unwrap();}
   checked(m.vcpu.set_guest_debug(&dbg),"Linux step arm")?;
   match m.vcpu.run(){Ok(VcpuExit::Debug(d))=>{require(d.exception==1&&d.dr6&(if breakpoint{1}else{1<<14})!=0,"Linux debugger exit")?;debug_exits+=1},
    Ok(VcpuExit::IoOut(0x530,d))=>{require(d.len()==1&&d[0]==3,"Linux correction PIO")?;let completion=io(&mut m.vcpu,&counter,true)?;boundaries.push(json!({"tag":3,"phase":"correction","count":counter.read()?[0],"rip":m.vcpu.get_regs().unwrap().rip,"completion":completion}));},
    Err(e) if e.errno()==libc::EINTR=>{eintr+=1;continue},x=>return Err(format!("Linux step {x:?}"))}
   steps+=1;
  }
  checked(m.vcpu.set_guest_debug(&kvm_guest_debug::default()),"Linux debug teardown")?;
 }

 let mut rf_cleanup:Option<Value>=None;
 if breakpoint {let mut r=m.vcpu.get_regs().unwrap();let before=r.rflags;r.rflags&=!(1<<16);m.vcpu.set_regs(&r).unwrap();rf_cleanup=Some(json!({"before":before,"after":r.rflags,"scope":"clear debugger-owned resume flag, fixture guest RF originally zero"}));}
 let correction_ns=correction_start.elapsed().as_nanos();
 let read=counter.read()?;let r=m.vcpu.get_regs().unwrap();
 let left=(1_000_000-progress) as u32;let flags=0x202u64|if (left as u8).count_ones()%2==0{4}else{0}|if left&15==15{16}else{0};
 let analytic=r.rax==progress&&r.rcx==left as u64&&r.rflags==flags;
 let debug_registers=format!("{:?}",m.vcpu.get_debug_regs());let vcpu_events=format!("{:?}",m.vcpu.get_vcpu_events());let valid=read[1]>0&&read[1]==read[2];
 let mut accepted=!correct||(landed&&!overshoot&&analytic&&valid&&read[0]==target&&r.rip==expected_rip&&debug_registers.starts_with("Ok(")&&vcpu_events.starts_with("Ok("));let mut error=None;
 let normal_stop_ns=correction_start.elapsed().as_nanos();let witness_start=std::time::Instant::now();
 let regs=regs_value(&r);let ram=hash(m.memory.bytes());let witness_ns=witness_start.elapsed().as_nanos();let elapsed=start.elapsed().as_nanos();
 let continuation_start=std::time::Instant::now();
 let terminal=if correct&&accepted{match finish(&mut m,&counter){Ok(t)=>Some(t),Err(e)=>{error=Some(e);accepted=false;None}}}else{None};
 let continuation_total_ns=continuation_start.elapsed().as_nanos();
 Ok(json!({"mode":"linux","target":target,"margin":margin,"correction":correct,"debugger_resume_flag_cleanup":rf_cleanup,"correction_method":if breakpoint{"hardware_RIP_breakpoint"}else if count_only{"count_only_single_step"}else{"known_RIP_single_step"},"overflow_elapsed_ns":overflow_elapsed,"correction_ns":correction_ns,"setup_ns":setup_ns,"restore_and_input_validation_ns":restore_ns,"initial_witness_ns":initial_witness_ns,"continuation_total_ns":continuation_total_ns,"normal_stop_overhead_ns":normal_stop_ns,"landing_witness_ns":witness_ns,"accepted":accepted,"landed":landed,"overshoot":overshoot,"analytic_valid":analytic,"perf_valid":valid,"expected_rip":expected_rip,"initial_registers":initial_registers,"initial_ram_sha256":initial_ram,"input_state":input_state,"overflow_count":overflow[0],"overflow_registers":overflow_regs,"final_count":read[0],"final_registers":regs,"registers_sha256":hash(&serde_json::to_vec(&regs).unwrap()),"ram_sha256":ram,"correction_steps":steps,"debug_exits":debug_exits,"pio_boundaries":boundaries,"eintr":eintr,"signals":SIGNALS.load(Ordering::Relaxed),"watchdog_alarms":ALARMS.load(Ordering::Relaxed),"host_admin_count_before":admin_before,"host_admin_count_after":admin_after,"time_enabled_ns":read[1],"time_running_ns":read[2],"elapsed_ns":elapsed,"perf_config":0x5100d1u64,"perf_flags":524293,"single_thread_tid":owner.tid,"tasks_before":tasks_before,"tasks_after":task_inventory()?,"debug_registers":debug_registers,"vcpu_events":vcpu_events,"terminal":terminal,"terminal_error":error}))
}
