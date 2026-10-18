use super::*;
#[test]
fn fragments_and_same_time_writes_survive_restore_and_drain_once() {
    let mut output = LifecycleOutput::default();
    output.push(7, b"rea".to_vec());
    output.push(7, b"dy\nnext\n".to_vec());
    output.push(9, b"tail".to_vec());
    let snapshot = serde_json::to_vec(&output).unwrap();
    let mut restored: LifecycleOutput = serde_json::from_slice(&snapshot).unwrap();
    let writes = restored.drain();
    assert_eq!(writes.iter().map(|w| w.time).collect::<Vec<_>>(), [7, 7, 9]);
    assert_eq!(
        writes
            .iter()
            .flat_map(|w| w.bytes.iter().copied())
            .collect::<Vec<_>>(),
        b"ready\nnext\ntail"
    );
    assert!(restored.drain().is_empty());
    let after = serde_json::to_vec(&restored).unwrap();
    let mut restored: LifecycleOutput = serde_json::from_slice(&after).unwrap();
    assert!(restored.drain().is_empty());
    assert_eq!(output.drain(), writes);
}
