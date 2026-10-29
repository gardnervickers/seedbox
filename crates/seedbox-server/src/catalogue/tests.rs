use super::*;
use seedbox_spec::definition::Override;
use serde_json::json;

fn spec(action: serde_json::Value) -> seedbox_api::SimulationSpec {
    let configuration = seedbox_application::config::parse(
        "name: test\nmachines:\n  db: {image: example:v1}\noracle: {image: example/oracle}\n",
        std::path::Path::new("/fixture/seedbox.yaml"),
        &Default::default(),
    )
    .unwrap();
    seedbox_api::SimulationSpec {
        seed: Some(1),
        inputs: InputSource::Project,
        configuration,
        program: vec![Override {
            id: "fault".into(),
            at_ns: 10,
            action,
            timeout_ns: seedbox_spec::definition::default_timeout_ns(),
        }],
    }
}

fn refusal(action: serde_json::Value) -> String {
    match resolve_program(&spec(action)) {
        Err(Error::Invalid(message)) => message,
        other => panic!("expected an invalid program, got {other:?}"),
    }
}

#[test]
fn submitted_programs_accept_disk_and_scheduler_faults_after_argument_checks() {
    for action in [
        json!({"action":"disk_error","handle":"eio","machine":"db","op":"write","ppm":10}),
        json!({"action":"disk_latency","handle":"slow","machine":"db","op":"any","latency_ns":20_000_000}),
        json!({"action":"preempt","handle":"race","machine":"db","ppm":10_000}),
    ] {
        let program = resolve_program(&spec(action.clone())).unwrap();
        assert_eq!(serde_json::to_value(&program[0].action).unwrap(), action);
    }
    // Argument errors come first, from the same schema.
    assert!(
        refusal(json!({"action":"preempt","handle":"race","machine":"db","ppm":0}))
            .contains("preempt ppm")
    );
    assert!(refusal(
        json!({"action":"disk_error","handle":"eio","machine":"nobody","op":"write","ppm":10})
    )
    .contains("unknown machine"));
}

#[test]
fn injected_faults_are_checked_before_journaling() {
    let inject = |action: serde_json::Value| seedbox_api::ControlCommand::InjectFault {
        action: serde_json::from_value(action).unwrap(),
    };
    check_injected(&inject(json!({"action":"crash","machine":"db"}))).unwrap();
    check_injected(&seedbox_api::ControlCommand::Step).unwrap();
    check_injected(&inject(
        json!({"action":"preempt","handle":"p","machine":"db","ppm":10}),
    ))
    .unwrap();
    assert!(matches!(
        check_injected(&inject(json!({"action":"preempt","handle":"p","machine":"db","ppm":0}))),
        Err(Error::Invalid(message)) if message.contains("preempt ppm")
    ));
}
