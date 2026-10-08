//! Actual World-path controls. These require KVM and the qualified counter;
//! the portable matcher tests do not substitute for these integration controls.
use super::*;
use crate::machine::Stop;
use seedbox_control::Action;

fn boot(marker:u8,undo:u64)->BootInputs {
    // One counted branch, console marker, 1000 counted branches, exit 0.
    let mut code=vec![0x31,0xc0,0x74,0x00,0xb0,marker,0xe6,0xe9,0xb9,0xe8,0x03,0,0,0x49,0x75,0xfd,0xb0,0,0xe6,0xf4];
    let mut kernel=vec![0;1024];kernel[0x1f1]=1;kernel.append(&mut code);
    // RAM must cover the fixed 128 MiB initrd address even with an empty initrd.
    BootInputs {kernel:kernel.into(),initrd:Rc::from([]),cmdline:(String::new(),String::new()),mem:0x0900_0000,mac:None,control_channel:false,disk:None,pv_clock:false,page_timer:false,undo_margin:undo,tolerance:0,timer_jitter:0,exact_timer_ppm:0,margin:None,console_limit:None,log_chunk_limit:None,deferred_tx:false}
}
fn world(undo:u64,reverse:bool)->World {
    world_with_counter(undo,reverse,Counter::new().unwrap())
}
fn world_with_counter(undo:u64,reverse:bool,mut counter:Counter)->World {
    // main.rs reuses one counter across forks. Counter has process-lifetime
    // ownership (no Drop); opening one per variant exhausts pinned PMU slots.
    // Disarm the previous world's overflow; counts are cumulative and each
    // KVM_RUN measures its own delta, so the counter needs no count reset.
    counter.disarm();
    let kvm=kvm_ioctls::Kvm::new().unwrap();
    let names:Vec<String>=if reverse {vec!["b".into(),"a".into()]} else {vec!["a".into(),"b".into()]};
    let boot:Vec<_>=names.iter().map(|n|boot(n.as_bytes()[0],undo)).collect();
    let machines=names.iter().zip(&boot).map(|(n,b)|boot_machine(&kvm,n,b,false,[0;32]).unwrap()).collect();
    World {failure:None,machines,factory:kvm,boot,barrier:false,boot_seed:1009,run_seed:1009,branch_identity:None,incarnations:vec![vec![],vec![]],paused_ingress:vec![0;2],clock:Clock::Counter(counter),events:vec![BTreeMap::new();2],seq:0,lookahead:50,early:0,latency:50,partitioned:BTreeMap::new(),faults:faults::Faults::with_links(vec![],names.clone(),links::Links::from_config(1009,&names,&json!({"latency_ns":50})).unwrap()).unwrap(),control:control::Control::new(&json!({"machine":"a","latency_ns":50,"placement":{}}),names,50,0).unwrap(),strict_events:vec![BTreeMap::new();2],macs:vec![[0,0,0,0,0,1],[0,0,0,0,0,2]],dropped:0,delivered:0,link_counts:BTreeMap::new(),semantic_counts:network_counters::NetworkCounters::default(),slices:0}
}
fn partition(handle:&str)->Action {Action::Partition {handle:handle.into(),left:vec!["a".into()],right:vec!["b".into()]}}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn committed_trigger_world_effects_follow_arm_order_across_reversed_machine_ties() {
    let mut want=None;
    let mut counter=Counter::new().unwrap();
    for reverse in [false,true] {for undo in [0,16,256] {
        let mut w=world_with_counter(undo,reverse,counter);
        w.control.triggers.arm("a".into(),vec![b'a'],0,5000,partition("first")).unwrap();
        w.control.triggers.arm("b".into(),vec![b'b'],0,5000,partition("second")).unwrap();
        let outcome=simulate(&mut w,&json!({}),2000,&None,None,&mut None).unwrap();
        assert_eq!(outcome,"quiescent");
        assert_eq!(w.faults.log.len(),2);
        assert_eq!(w.faults.log[0]["request"]["handle"],"first");
        assert_eq!(w.faults.log[1]["request"]["handle"],"second");
        assert!(w.control.triggers.records.iter().all(|r|r.matched_at_ns==Some(1) && r.effect_at_ns==Some(51) && r.status=="fired"));
        let evidence=w.control.triggers.evidence().unwrap();
        if let Some(want)=&want {assert_eq!(&evidence,want);} else {want=Some(evidence);}
        let Clock::Counter(c)=w.clock else {unreachable!("these worlds use the counter")};
        counter=c;
    }}
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn committed_trigger_world_retains_already_routed_response_and_lagging_peer() {
    let mut w=world(256,false);
    // The peer begins a charged timer-port read. Its sampled response is
    // already committed, while its debt must survive the trigger barrier.
    let mut peer_boot=boot(b'b',256);
    let mut kernel=peer_boot.kernel.to_vec();
    kernel.splice(1024..1024,[0xba,0x61,0,0,0,0xec]);
    peer_boot.kernel=kernel.into();
    w.machines[1]=boot_machine(&w.factory,"b",&peer_boot,false,[0;32]).unwrap();
    // A delayed, already-routed response stays queued beyond the future cut.
    w.latency=1000;
    w.faults.links.global.latency_ns=1000;
    let mut response=vec![0;14];response[..6].copy_from_slice(&w.macs[1]);
    w.route(0,0,response.clone());
    let queued=w.events[1].clone();
    w.control.triggers.arm("a".into(),vec![b'a'],0,5000,partition("p")).unwrap();
    let mut frames=Vec::new();
    w.machines[0].observe_console=true;
    let Clock::Counter(c)=&mut w.clock else {unreachable!("these worlds use the counter")};
    assert_eq!(w.machines[0].run_until(c,50,0,&mut frames).unwrap(),Stop::Control);
    let (at,byte)=w.machines[0].committed_console.take().unwrap();
    assert_eq!(w.machines[1].time,0,"peer has not caught up to the marker");
    w.control.triggers.committed("a",at,byte,50,w.machines[0].time).unwrap();
    assert!(!w.faults.blocked(0,1));
    // Let the real scheduler catch every active machine up to the barrier.
    simulate(&mut w,&json!({}),52,&None,None,&mut None).unwrap();
    assert_eq!(w.machines[0].time,52);assert_eq!(w.machines[1].time,52);
    assert!(w.faults.blocked(0,1));
    assert_eq!(w.machines[1].pending_io.as_ref().unwrap().remaining_ns,948);
    assert_eq!(w.events[1].keys().collect::<Vec<_>>(),queued.keys().collect::<Vec<_>>());
    assert!(matches!(w.events[1].values().next(),Some(Payload::Frame{bytes,..}) if bytes==&response));
    w.route(0,52,response);
    assert_eq!(w.dropped,1);assert_eq!(w.events[1].len(),1);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn committed_trigger_world_snapshot_keeps_partial_match_and_scheduled_effect() {
    let mut counter=Counter::new().unwrap();
    for pending in [false,true] {
        let mut w=world_with_counter(256,false,counter);
        w.control.triggers.arm("a".into(),b"ab".to_vec(),0,5000,partition("p")).unwrap();
        w.control.triggers.committed("a",1,b'a',50,1).unwrap();
        if pending {w.control.triggers.committed("a",1,b'b',50,1).unwrap();}
        for m in &mut w.machines {m.park().unwrap();}
        let snapshot=WorldSnapshot::take(&mut w).unwrap();
        let want=w.control.triggers.evidence().unwrap();
        w.control.triggers.cut_off(10);
        snapshot.restore(&mut w).unwrap();
        assert_eq!(w.control.triggers.evidence().unwrap(),want);
        if !pending {w.control.triggers.committed("a",1,b'b',50,1).unwrap();}
        w.control.triggers.apply_boundary(&mut w.faults,51).unwrap();
        assert_eq!(w.faults.log.len(),1);
        snapshot.restore(&mut w).unwrap();
        assert_eq!(w.control.triggers.evidence().unwrap(),want);
        assert!(w.faults.log.is_empty());
        let Clock::Counter(c)=w.clock else {unreachable!("these worlds use the counter")};
        counter=c;
    }
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn committed_trigger_unused_surface_preserves_witness_and_report_bytes() {
    let mut w=world(256,false);
    let outcome=simulate(&mut w,&json!({}),2000,&None,None,&mut None).unwrap();
    let out=std::env::temp_dir().join(format!("seedbox-trigger-unused-{}",std::process::id()));
    std::fs::create_dir(&out).unwrap();
    let before=report(&mut w,outcome,1009,&[],1.0,true,out.to_str().unwrap(),2000,&None).unwrap();
    w.control.triggers.committed("a",100,b'x',50,100).unwrap();
    w.control.triggers.reset_source("a");w.control.triggers.cut_off(2000);
    let after=report(&mut w,outcome,1009,&[],1.0,true,out.to_str().unwrap(),2000,&None).unwrap();
    assert!(after.get("triggers").is_none());
    assert_eq!(serde_json::to_vec(&before).unwrap(),serde_json::to_vec(&after).unwrap());
    std::fs::remove_dir_all(out).unwrap();
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn committed_trigger_world_does_not_rescan_old_console_after_arm() {
    let mut w=world(256,false);let mut frames=Vec::new();
    let Clock::Counter(c)=&mut w.clock else {unreachable!("these worlds use the counter")};
    assert_eq!(w.machines[0].run_until(c,50,0,&mut frames).unwrap(),Stop::Reached);
    assert_eq!(w.machines[0].com1.out,b"a");
    assert_eq!(w.machines[0].committed_console,None);
    w.control.triggers.arm("a".into(),vec![b'a'],50,5000,partition("p")).unwrap();
    simulate(&mut w,&json!({}),2000,&None,None,&mut None).unwrap();
    assert!(w.faults.log.is_empty());
    assert_eq!(w.control.triggers.records[0].prefix,0);
    assert_eq!(w.control.triggers.records[0].seen,0);
    assert_eq!(w.control.triggers.records[0].matched_at_ns,None);
}
