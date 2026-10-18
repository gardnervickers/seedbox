//! Portable checkpoint input metadata. The captured Driver input cursor, with
//! its admissions and rejections, is authoritative; it is validated, never
//! rewritten.
use crate::{Error, Result};
use seedbox_engine_protocol::driver_world::Program;
use seedbox_spec::definition::Start;

/// Validate a checkpoint's captured input cursor without a backend.
pub fn validate(start: &Start) -> Result<()> {
    let Start::Checkpoint { manifest, .. } = start else {
        return Ok(());
    };
    let value = manifest
        .pointer("/state/world/input_program")
        .ok_or_else(|| Error::Integrity("checkpoint is missing its input program".into()))?;
    let program: Program = serde_json::from_value(value.clone())?;
    program.validate().map_err(Error::Integrity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(program: serde_json::Value) -> Start {
        Start::Checkpoint {
            manifest: json!({"state":{"world":{"input_program":program}}}),
            assets: Default::default(),
        }
    }

    fn input(key: &str, at_ns: u64) -> serde_json::Value {
        json!({"key":key,"at_ns":at_ns,"action":{"action":"pause_process","machine":"oracle"},"timeout_ns":100})
    }

    #[test]
    fn captured_driver_cursor_is_validated() {
        validate(&Start::Fresh).unwrap();
        validate(&start(
            json!({"inputs":[],"next":0,"admissions":[],"rejections":[]}),
        ))
        .unwrap();
        let admitted = json!({
            "inputs":[input("first", 10), input("second", 20), input("pending", 30)],
            "next":2,
            "admissions":[{"key":"first","plan":{"id":1,"at_ns":10,"deadline_ns":110,"action":"pause_process","machine":"oracle"}}],
            "rejections":[{"key":"second","at_ns":20,"reason":"rejected"}],
        });
        validate(&start(admitted.clone())).unwrap();

        let mut cursor = admitted.clone();
        cursor["next"] = 3.into();
        assert!(validate(&start(cursor)).is_err());
        let mut identity = admitted.clone();
        identity["admissions"][0]["plan"]["at_ns"] = 11.into();
        assert!(validate(&start(identity)).is_err());
        let mut legacy = admitted;
        legacy["failed"] = false.into();
        assert!(validate(&start(legacy)).is_err());
        assert!(validate(&Start::Checkpoint {
            manifest: json!({"state":{"world":{}}}),
            assets: Default::default(),
        })
        .is_err());
    }
}
