//! Fault metadata belongs to the server, alongside control admission.
use seedbox_api::{FaultCatalogue, FaultDescriptor, WorldState};
use seedbox_control::{
    MAX_CLOCK_OFFSET_NS, MAX_CLOCK_RATE_PPB, MAX_LINK_DELAY_NS, MAX_PPM, THROTTLE_PERMILLE,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn integer(
    description: &str,
    minimum: i128,
    maximum: u64,
    default: Option<i64>,
    unit: &str,
) -> Value {
    let mut value = json!({"type":"integer", "description":description, "minimum": minimum as i64, "maximum":maximum, "x-unit":unit});
    // Input syntax is discoverable metadata; wire values remain canonical integers.
    let input = match unit {
        "ns" => Some((
            json!({"ns":1,"us":1000,"µs":1000,"ms":1_000_000,"s":1_000_000_000,"m":60_000_000_000u64,"h":3_600_000_000_000u64}),
            "100ms",
            "ms",
        )),
        "ppm" => Some((json!({"ppm":1,"%":10_000}), "2.5%", "%")),
        "ppb" => Some((json!({"ppb":1,"ppm":1000,"%":10_000_000}), "100ppm", "ppm")),
        "permille" => Some((json!({"permille":1,"%":10,"x":1000}), "1.5x", "x")),
        "bit/s" => Some((
            json!({"bit/s":1,"kbit/s":1000,"Mbit/s":1_000_000,"Gbit/s":1_000_000_000}),
            "10Mbit/s",
            "Mbit/s",
        )),
        _ => None,
    };
    if let Some((units, example, display)) = input {
        value["x-input-units"] = units;
        value["x-input-example"] = json!(example);
        value["x-display-unit"] = json!(display);
    }
    if let Some(default) = default {
        value["default"] = json!(default);
    }
    value
}
fn choice(description: &str, choices: &[String]) -> Value {
    json!({"type":"string", "description":description, "enum":choices})
}
fn group(description: &str, choices: &[String]) -> Value {
    json!({"type":"array", "description":description, "minItems":1, "uniqueItems":true, "items":choice("Machine", choices)})
}
fn descriptor(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> FaultDescriptor {
    let mut properties = properties;
    properties["action"] =
        json!({"type":"string", "const":name, "default":name, "description":"Fault action"});
    let mut required = required.iter().map(|s| json!(s)).collect::<Vec<_>>();
    required.insert(0, json!("action"));
    FaultDescriptor {
        name: name.into(),
        title: title.into(),
        description: description.into(),
        schema: json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "title":title,
            "description":description, "properties":properties, "required":required, "additionalProperties":false
        }),
    }
}

pub(crate) fn discover(world: &WorldState, minimum_latency_ns: u64) -> FaultCatalogue {
    let machines = world
        .machines
        .iter()
        .map(|m| m.name.clone())
        .collect::<Vec<_>>();
    let sources = world
        .links
        .iter()
        .map(|l| l.from.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let destinations = world
        .links
        .iter()
        .map(|l| l.to.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let handles = world
        .faults
        .iter()
        .filter(|f| f.active)
        .filter_map(|f| {
            let plan: seedbox_control::Action = serde_json::from_value(f.plan.clone()?).ok()?;
            match plan {
                seedbox_control::Action::Partition { handle, .. }
                | seedbox_control::Action::Shape { handle, .. }
                | seedbox_control::Action::ClockSkew { handle, .. }
                | seedbox_control::Action::ClockDrift { handle, .. }
                | seedbox_control::Action::Throttle { handle, .. } => Some(handle.clone()),
                _ => None,
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let machine = choice("Machine to affect", &machines);
    let handle = json!({"type":"string", "minLength":1, "description":"Unique fault handle used to heal this fault"});
    let mut faults = Vec::new();
    for (name, title, description) in [
        (
            "stop",
            "Stop application",
            "Stop the application's process group.",
        ),
        (
            "restart",
            "Restart application",
            "Restart the application process.",
        ),
        (
            "pause_process",
            "Pause application",
            "Send SIGSTOP to the application process group; its kernel keeps running.",
        ),
        (
            "resume_process",
            "Resume application",
            "Send SIGCONT to a paused application process group.",
        ),
        (
            "pause",
            "Pause machine",
            "Freeze the machine's CPU and devices while the world continues.",
        ),
        (
            "resume",
            "Resume machine",
            "Continue a paused machine, catching up timers and held ingress.",
        ),
        (
            "crash",
            "Crash machine",
            "Destroy the VM's RAM, devices, queued I/O and sockets.",
        ),
        (
            "reboot",
            "Reboot machine",
            "Boot a crashed machine as a new incarnation.",
        ),
    ] {
        faults.push(descriptor(
            name,
            title,
            description,
            json!({"machine":machine}),
            &["machine"],
        ));
    }
    faults.push(descriptor("partition", "Partition network", "Cut traffic between two disjoint groups of machines. Every selected pair must have a configured link.",
        json!({"handle":handle, "left":group("First group of machines", &machines), "right":group("Second disjoint group of machines", &machines)}), &["handle","left","right"]));
    let mut network = json!({});
    for (name, description) in [
        (
            "reorder_ppm",
            "Probability of delaying a frame to reorder it",
        ),
        ("duplicate_ppm", "Probability of duplicating a frame"),
        (
            "burst_loss_ppm",
            "Probability of starting a burst of dropped frames",
        ),
        ("latency_spike_ppm", "Probability of adding a latency spike"),
        ("tcp_reset_ppm", "Probability of resetting a TCP connection"),
    ] {
        network[name] = integer(description, 0, MAX_PPM, Some(0), "ppm");
    }
    for (name, description) in [
        ("reorder_delay_ns", "Extra delay for reordered frames"),
        ("latency_spike_ns", "Extra delay for latency spikes"),
    ] {
        network[name] = integer(description, 0, MAX_LINK_DELAY_NS, Some(0), "ns");
    }
    network["burst_length"] = integer(
        "Frames to drop in a loss burst",
        0,
        u64::MAX,
        Some(0),
        "frames",
    );
    let mut network_schema = json!({"type":"object", "description":"Seeded send-time impairments. Active reorder, burst loss and latency spike faults require a positive amount. Total latency, jitter and active extra delays must not exceed one hour.","default":{}, "properties":network, "additionalProperties":false});
    network_schema["allOf"] = json!([
        {"if":{"properties":{"reorder_ppm":{"minimum":1}},"required":["reorder_ppm"]},"then":{"properties":{"reorder_delay_ns":{"minimum":1}},"required":["reorder_delay_ns"]}},
        {"if":{"properties":{"burst_loss_ppm":{"minimum":1}},"required":["burst_loss_ppm"]},"then":{"properties":{"burst_length":{"minimum":1}},"required":["burst_length"]}},
        {"if":{"properties":{"latency_spike_ppm":{"minimum":1}},"required":["latency_spike_ppm"]},"then":{"properties":{"latency_spike_ns":{"minimum":1}},"required":["latency_spike_ns"]}}
    ]);
    faults.push(descriptor("shape", "Shape network", "Apply a healable directed network policy. Every selected source/destination pair must have a configured link. Latency must be at least the global latency, jitter must be less than latency, and total latency, jitter and active extra delays must not exceed one hour.", json!({
        "handle":handle, "from":group("Source machines with configured outgoing links", &sources), "to":group("Destination machines with configured incoming links", &destinations),
        "latency_ns":integer("Fixed frame delay",minimum_latency_ns as i128,MAX_LINK_DELAY_NS,Some(minimum_latency_ns.max(1) as i64),"ns"),
        "jitter_ns":integer("Seeded additional frame delay",0,MAX_LINK_DELAY_NS,Some(0),"ns"),
        "loss_ppm":integer("Frame loss probability",0,MAX_PPM,Some(0),"ppm"),
        "bandwidth_bps":integer("Optional link bandwidth",1,u64::MAX,None,"bit/s"),
        "queue_bound_ns":integer("Optional maximum queued time",0,u64::MAX,None,"ns"),
        "network_faults":network_schema
    }), &["handle","from","to","latency_ns","jitter_ns","loss_ppm"]));
    let shape = faults.last_mut().expect("shape descriptor");
    shape.schema["dependentRequired"] = json!({"queue_bound_ns":["bandwidth_bps"]});
    faults.push(descriptor(
        "heal",
        "Heal fault",
        "Remove an active healable fault by handle.",
        json!({"handle":choice("Active fault handle", &handles)}),
        &["handle"],
    ));
    for (name,title,description,fixed,bound,max,unit) in [
        ("clock_skew","Skew wall clock","Step CLOCK_REALTIME; heal reverses the offset. Choose exactly one fixed nonzero offset or a positive seeded bound.","offset_ns","max_offset_ns",MAX_CLOCK_OFFSET_NS,"ns"),
        ("clock_drift","Drift system clock","Change realtime and monotonic clock rate; heal restores nominal rate. Choose exactly one fixed nonzero rate or a positive seeded bound.","rate_ppb","max_rate_ppb",MAX_CLOCK_RATE_PPB,"ppb"),
    ] {
        let mut properties = json!({"handle":handle,"machine":machine});
        properties[fixed] = integer("Fixed signed nonzero amount",-(max as i128),max,None,unit);
        properties[fixed]["not"] = json!({"const":0});
        properties[bound] = integer("Positive bound for a seeded amount drawn from minus to plus this value",1,max,None,unit);
        let mut fault = descriptor(name,title,description,properties,&["handle","machine"]);
        fault.schema["oneOf"] = json!([{"required":[fixed],"not":{"required":[bound]}},{"required":[bound],"not":{"required":[fixed]}}]);
        faults.push(fault);
    }
    faults.push(descriptor("throttle","Throttle CPU","Scale the machine's seeded CPU cost until healed.",json!({"handle":handle,"machine":machine,"factor_permille":integer("CPU cost multiplier in thousandths",*THROTTLE_PERMILLE.start() as i128,*THROTTLE_PERMILLE.end(),Some(1000),"permille")}),&["handle","machine","factor_permille"]));
    faults.push(descriptor("drop_connection", "Drop TCP connection", "Reset one observed IPv4 TCP incarnation. Supply the exact addresses, ports and incarnation from connection observations; this world view does not enumerate live connections.", json!({"handle":handle, "connection":{
        "type":"object", "description":"Exact directed TCP connection identity", "additionalProperties":false,
        "properties":{
            "from":choice("Source machine", &sources), "to":choice("Destination machine", &destinations),
            "src_addr":{"type":"string","format":"ipv4","description":"Observed source IPv4 address"},
            "dst_addr":{"type":"string","format":"ipv4","description":"Observed destination IPv4 address"},
            "src_port":integer("Source TCP port",1,65535,None,"port"),
            "dst_port":integer("Destination TCP port",1,65535,None,"port"),
            "incarnation":integer("Observed connection incarnation",1,u64::MAX,None,"incarnation")
        }, "required":["from","to","src_addr","dst_addr","src_port","dst_port","incarnation"]
    }}), &["handle","connection"]));
    FaultCatalogue {
        simulation: world.simulation.clone(),
        faults,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_control::Action;

    // Exhaustive match intentionally makes adding a variant require reviewing
    // its discoverable form, rather than silently omitting a new engine action.
    fn recognized(action: &Action) {
        match action {
            Action::Stop { .. }
            | Action::Restart { .. }
            | Action::PauseProcess { .. }
            | Action::ResumeProcess { .. }
            | Action::Partition { .. }
            | Action::Shape { .. }
            | Action::Heal { .. }
            | Action::ClockSkew { .. }
            | Action::ClockDrift { .. }
            | Action::Throttle { .. }
            | Action::Pause { .. }
            | Action::Resume { .. }
            | Action::Crash { .. }
            | Action::Reboot { .. }
            | Action::DropConnection { .. } => {}
        }
    }
    fn example(schema: &Value) -> Value {
        if let Some(value) = schema.get("const").or_else(|| schema.get("default")) {
            return value.clone();
        }
        if let Some(choices) = schema["enum"].as_array() {
            return choices.first().cloned().unwrap_or(json!("test"));
        }
        match schema["type"].as_str().unwrap() {
            "object" => {
                let mut value = json!({});
                for name in schema["required"].as_array().into_iter().flatten() {
                    let name = name.as_str().unwrap();
                    value[name] = example(&schema["properties"][name]);
                }
                if let Some(first) = schema["oneOf"].as_array().and_then(|v| v.first()) {
                    for name in first["required"].as_array().unwrap() {
                        let name = name.as_str().unwrap();
                        value[name] = example(&schema["properties"][name]);
                    }
                }
                value
            }
            "array" => json!([example(&schema["items"])]),
            "integer" => {
                let minimum = schema["minimum"].as_i64().unwrap();
                json!(if schema["not"]["const"] == 0 {
                    1
                } else {
                    minimum
                })
            }
            "string" => {
                if schema["format"] == "ipv4" {
                    json!("10.0.0.1")
                } else {
                    json!("test")
                }
            }
            other => panic!("missing example for {other}"),
        }
    }
    #[test]
    fn every_form_generates_a_real_action_with_serializable_defaults() {
        let world = WorldState {
            override_rejections: Default::default(),
            simulation: "abcdefgh".parse().unwrap(),
            revision: 1,
            simulated_time_ns: 0,
            machines: vec![],
            links: vec![],
            faults: vec![],
        };
        let catalogue = discover(&world, 1_000_000);
        assert_eq!(catalogue.faults.len(), 15);
        let mut names = BTreeSet::new();
        for descriptor in &catalogue.faults {
            assert!(names.insert(&descriptor.name));
            let value = example(&descriptor.schema);
            let mut value = value;
            if descriptor.name == "shape" {
                // Exercise every nested impairment default, including fields
                // serde normally omits when the impairment is disabled.
                let properties = descriptor.schema["properties"]["network_faults"]["properties"]
                    .as_object()
                    .unwrap();
                let mut network = json!({});
                for (name, property) in properties {
                    network[name] = example(property);
                }
                value["network_faults"] = network;
            }
            let action: Action = serde_json::from_value(value.clone())
                .unwrap_or_else(|e| panic!("{}: {e}: {value}", descriptor.name));
            recognized(&action);
            assert_eq!(action.name(), descriptor.name);
            assert!(!descriptor.title.is_empty() && !descriptor.description.is_empty());
            assert_eq!(
                descriptor.schema["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
            let wire = serde_json::to_value(&action).unwrap();
            assert_eq!(serde_json::from_value::<Action>(wire).unwrap(), action);
        }
    }
}

#[cfg(test)]
mod unit_metadata_tests {
    use super::*;

    #[test]
    fn input_units_describe_conversion_without_changing_wire_defaults_or_bounds() {
        let duration = integer("Delay", 1, MAX_LINK_DELAY_NS, Some(1_000_000), "ns");
        assert_eq!(duration["default"], 1_000_000);
        assert_eq!(duration["minimum"], 1);
        assert_eq!(duration["maximum"], MAX_LINK_DELAY_NS);
        assert_eq!(duration["x-input-units"]["ms"], 1_000_000);
        assert_eq!(duration["x-display-unit"], "ms");
        assert_eq!(
            integer("Loss", 0, MAX_PPM, Some(0), "ppm")["x-input-units"]["%"],
            10_000
        );
        assert_eq!(
            integer("Clock", -1, MAX_CLOCK_RATE_PPB, None, "ppb")["x-input-units"]["ppm"],
            1000
        );
        assert_eq!(
            integer("Multiplier", 1, 10_000, Some(1000), "permille")["x-input-units"]["x"],
            1000
        );
        assert!(integer("Port", 1, 65535, None, "port")
            .get("x-input-units")
            .is_none());
    }
}
