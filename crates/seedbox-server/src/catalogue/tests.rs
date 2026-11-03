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
fn submitted_programs_check_arguments_then_refuse_faults_no_engine_provides() {
    for (action, message) in [
        (
            json!({"action":"disk_error","handle":"eio","machine":"db","op":"write","ppm":10}),
            "disk_error needs disk request selection",
        ),
        (
            json!({"action":"disk_latency","handle":"slow","machine":"db","op":"any","latency_ns":20_000_000}),
            "disk_latency needs delayed block completion",
        ),
        (
            json!({"action":"preempt","handle":"race","machine":"db","ppm":10_000}),
            "preempt needs the guest yield hook",
        ),
    ] {
        assert_eq!(
            refusal(action),
            format!("program fault: {message}, which no engine provides")
        );
    }
    // Argument errors come first, from the same schema.
    assert!(
        refusal(json!({"action":"preempt","handle":"race","machine":"db","ppm":0}))
            .contains("preempt ppm")
    );
}

#[test]
fn injected_faults_are_checked_before_journaling() {
    let inject = |action: serde_json::Value| seedbox_api::ControlCommand::InjectFault {
        action: serde_json::from_value(action).unwrap(),
    };
    check_injected(&inject(json!({"action":"crash","machine":"db"}))).unwrap();
    check_injected(&seedbox_api::ControlCommand::Step).unwrap();
    assert!(matches!(
        check_injected(&inject(json!({"action":"preempt","handle":"p","machine":"db","ppm":0}))),
        Err(Error::Invalid(message)) if message.contains("preempt ppm")
    ));
}
