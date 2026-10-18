//! Summarize a `seedbox explore --json` document: one row per member (its
//! oracle property's status, simulated time and verdict text, and any other
//! property that did not pass), then a tally by phase, termination reason and
//! oracle status. An inconclusive oracle verdict still counts as a passed
//! property, so the verdict text is what tells it apart. A crash that is
//! keel-s3dd's wedge watchdog aborting the process (by design, after a long
//! process pause) is named as such, and a member whose only failures are
//! those is tallied apart; whether each followed a pause past the watchdog's
//! bound is for the member's `seedbox explain` timeline to show.
use serde_json::Value;
use std::collections::BTreeMap;

/// One explore member, as the summary shows it.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub phase: String,
    pub termination: String,
    /// The oracle property's status, or `none` when the run has no oracle property.
    pub oracle: String,
    pub simulated_seconds: f64,
    pub verdict: String,
    /// Properties, the oracle's included, whose status is not `passed`.
    pub failing: Vec<String>,
    /// Every failing property is a keel-s3dd watchdog abort.
    pub watchdog_only: bool,
}

/// The failing-property label for a crash that is keel-s3dd's watchdog.
const WATCHDOG: &str = "seedbox.no_crashing_processes (keel-watchdog abort)";

fn label(property: &Value) -> String {
    let id = text(&property["id"]);
    let message = property["example"]["details"]["message"]
        .as_str()
        .unwrap_or("");
    if id == "seedbox.no_crashing_processes" && message.contains("keel-watchdog") {
        WATCHDOG.into()
    } else {
        id
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("-").to_owned()
}

pub fn rows(document: &Value) -> Vec<Row> {
    let members = document["members"].as_array().cloned().unwrap_or_default();
    members
        .iter()
        .map(|member| {
            let status = &member["status"];
            let properties = status["properties"]["properties"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            // Seedbox's default properties are named `seedbox.*`; the oracle's is the other one.
            let oracle = properties
                .iter()
                .find(|p| !p["id"].as_str().unwrap_or("").starts_with("seedbox."));
            let failing: Vec<String> = properties
                .iter()
                .filter(|p| p["status"].as_str() != Some("passed"))
                .map(label)
                .collect();
            Row {
                watchdog_only: !failing.is_empty() && failing.iter().all(|f| f == WATCHDOG),
                failing,
                id: text(&member["metadata"]["id"]),
                phase: text(&status["phase"]),
                termination: text(&status["termination"]["reason"]),
                oracle: oracle.map_or("none".into(), |p| text(&p["status"])),
                simulated_seconds: status["simulatedTimeNs"].as_f64().unwrap_or(0.0) / 1e9,
                verdict: oracle.map_or("-".into(), |p| text(&p["example"]["details"]["verdict"])),
            }
        })
        .collect()
}

pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    let mut tally: BTreeMap<(&str, &str, &str, &str), usize> = BTreeMap::new();
    for row in rows {
        let aborts = if row.watchdog_only {
            " with only keel-watchdog aborts"
        } else {
            ""
        };
        *tally
            .entry((&row.phase, &row.termination, &row.oracle, aborts))
            .or_default() += 1;
        let verdict: String = row.verdict.chars().take(110).collect();
        out.push_str(&format!(
            "{} {} {:.3} {}",
            row.id, row.oracle, row.simulated_seconds, verdict
        ));
        if !row.failing.is_empty() {
            out.push_str(&format!(" [{}]", row.failing.join(", ")));
        }
        out.push('\n');
    }
    for ((phase, termination, oracle, aborts), count) in tally {
        out.push_str(&format!("{count} {phase} {termination} {oracle}{aborts}\n"));
    }
    out
}

/// Summarize the document at `path`, or standard input for `-`.
pub fn run(path: &std::path::Path) -> u8 {
    let read = if path.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin())
    } else {
        std::fs::read_to_string(path)
    };
    let document: Value = match read
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(document) => document,
        Err(error) => {
            eprintln!("error: {}: {error}", path.display());
            return 2;
        }
    };
    print!("{}", render(&rows(&document)));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member(id: &str, oracle: &str, verdict: &str, oom: &str) -> Value {
        json!({
            "metadata": {"id": id},
            "status": {
                "phase": "completed",
                "termination": {"reason": "oracleExited"},
                "simulatedTimeNs": 121_500_000_000u64,
                "properties": {"properties": [
                    {"id": "seedbox.no_out_of_memory_kills", "status": oom},
                    {"id": "rustfs: check", "status": oracle,
                     "example": {"details": {"verdict": verdict}}},
                ]},
            },
        })
    }

    #[test]
    fn rows_name_the_oracle_and_every_failing_property() {
        let document = json!({"members": [
            member("aaaa", "passed", "ok: 10 objects", "passed"),
            member("bbbb", "unmet", "", "unmet"),
        ]});
        let rows = rows(&document);
        assert_eq!(rows[0].oracle, "passed");
        assert_eq!(rows[0].verdict, "ok: 10 objects");
        assert!(rows[0].failing.is_empty());
        assert_eq!(
            rows[1].failing,
            ["seedbox.no_out_of_memory_kills", "rustfs: check"]
        );
        assert_eq!(
            render(&rows),
            "aaaa passed 121.500 ok: 10 objects\n\
             bbbb unmet 121.500  [seedbox.no_out_of_memory_kills, rustfs: check]\n\
             1 completed oracleExited passed\n\
             1 completed oracleExited unmet\n"
        );
    }

    #[test]
    fn keel_watchdog_aborts_are_named_and_tallied_apart() {
        let mut aborted = member("dddd", "passed", "ok", "passed");
        aborted["status"]["properties"]["properties"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "seedbox.no_crashing_processes", "status": "violated",
                         "example": {"details": {"message": "libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6."}}}));
        let mut other = member("eeee", "passed", "ok", "passed");
        other["status"]["properties"]["properties"]
            .as_array_mut()
            .unwrap()
            .push(
                json!({"id": "seedbox.no_crashing_processes", "status": "violated",
                         "example": {"details": {"message": "n1: segfault"}}}),
            );
        let rows = rows(&json!({"members": [aborted, other]}));
        assert!(rows[0].watchdog_only && !rows[1].watchdog_only);
        let text = render(&rows);
        assert!(text.contains("1 completed oracleExited passed with only keel-watchdog aborts\n"));
        assert!(text.contains("eeee passed 121.500 ok [seedbox.no_crashing_processes]"));
    }

    #[test]
    fn a_stopped_member_without_properties_has_no_oracle() {
        let document = json!({"members": [{
            "metadata": {"id": "cccc"},
            "status": {"phase": "completed", "termination": {"reason": "stopped"}},
        }]});
        let rows = rows(&document);
        assert_eq!(
            (rows[0].oracle.as_str(), rows[0].verdict.as_str()),
            ("none", "-")
        );
    }
}
