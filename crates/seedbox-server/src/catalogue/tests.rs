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
fn submitted_programs_refuse_defined_only_faults_and_triggers_by_name() {
    let not_implemented = seedbox_engine_protocol::not_implemented;
    for (action, name) in [
        (
            json!({"action":"disk_error","handle":"eio","machine":"db","op":"write","ppm":10}),
            "disk_error",
        ),
        (
            json!({"action":"disk_latency","handle":"slow","machine":"db","op":"any","latency_ns":20_000_000}),
            "disk_latency",
        ),
        (
            json!({"action":"preempt","handle":"race","machine":"db","ppm":10_000}),
            "preempt",
        ),
        (
            json!({"action":"trigger","when":{"kind":"lifecycle","machine":"db","transition":"crashed"},"then":{"action":"reboot","machine":"db"}}),
            "trigger",
        ),
    ] {
        let message = match resolve_program(&spec(action)) {
            Err(Error::NotImplemented(message)) => message,
            other => panic!("expected a not-implemented program, got {other:?}"),
        };
        assert_eq!(message, format!("program fault: {}", not_implemented(name)));
    }
    // Argument and shape errors come first, from the same schema.
    assert!(
        refusal(json!({"action":"preempt","handle":"race","machine":"db","ppm":0}))
            .contains("preempt ppm")
    );
    assert!(refusal(json!({"action":"trigger","when":{"kind":"frame"},"nth":0,"then":{"action":"crash","machine":"db"}}))
        .contains("nth counts from 1"));
    assert!(refusal(json!({"action":"trigger","when":{"kind":"meteor","name":"x"},"then":{"action":"crash","machine":"db"}}))
        .contains("unknown variant"));
    // SDK evidence selectors parse, and their triggers are refused by name.
    assert!(matches!(
        resolve_program(&spec(json!({"action":"trigger","when":{"kind":"event","name":"x"},"then":{"action":"crash","machine":"db"}}))),
        Err(Error::NotImplemented(message)) if message.ends_with(&not_implemented("trigger"))
    ));
}

#[test]
fn injected_faults_no_engine_implements_are_refused_before_journaling() {
    let inject = |action: serde_json::Value| seedbox_api::ControlCommand::InjectFault {
        action: serde_json::from_value(action).unwrap(),
    };
    check_injected(&inject(json!({"action":"crash","machine":"db"}))).unwrap();
    check_injected(&seedbox_api::ControlCommand::Step).unwrap();
    assert!(matches!(
        check_injected(&inject(json!({"action":"preempt","handle":"p","machine":"db","ppm":10}))),
        Err(Error::NotImplemented(message)) if message == "preempt is not implemented yet"
    ));
    assert!(matches!(
        check_injected(&inject(json!({"action":"preempt","handle":"p","machine":"db","ppm":0}))),
        Err(Error::Invalid(message)) if message.contains("preempt ppm")
    ));
}

#[test]
fn project_programs_may_not_use_reserved_keys() {
    let mut project = spec(json!({"action":"crash","machine":"db"}));
    project.program[0].id = "gen-4".into();
    match resolve_program(&project) {
        Err(Error::Invalid(message)) => {
            assert!(
                message.contains("reserved for inputs Seedbox generates"),
                "{message}"
            )
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}
