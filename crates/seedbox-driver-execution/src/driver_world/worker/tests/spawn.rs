use super::*;

#[test]
fn authored_spawn_publishes_the_standby_name_and_public_plan() {
    let (_root, mut worker, _) = worker_with(1, false, vec![definition("standby")]);
    worker.step(Timestamp::from_nanos(10)).unwrap();
    worker.state.world.input_program.inputs =
        vec![seedbox_engine_protocol::input_program::TimedInput {
            key: "spawn".into(),
            at_ns: 10,
            timeout_ns: 5_000_000_000,
            action: seedbox_engine_protocol::control::Action::Spawn {
                machine: "standby".into(),
            },
        }];
    assert!(worker.authored_next().unwrap());
    assert!(worker.state.world.input_program.rejections.is_empty());
    worker.step(Timestamp::from_nanos(40)).unwrap();
    assert!(worker.names.values().any(|name| name == "standby"));
    let faults: Vec<_> = worker
        .publisher
        .published
        .iter()
        .filter(|event| event.to_string().contains("\"spawn\""))
        .collect();
    assert!(!faults.is_empty(), "{:?}", worker.publisher.published);
    assert!(faults
        .iter()
        .all(|event| event.to_string().contains("standby")));
}
