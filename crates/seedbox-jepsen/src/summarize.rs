//! Summarize a `seedbox explore --json` document: one row per member (its
//! oracle property's status, simulated time and verdict text, and any other
//! property that did not pass), then a tally by phase, termination reason and
//! oracle status. An inconclusive oracle verdict still counts as a passed
//! property, so the verdict text is what tells it apart.
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
            Row {
                id: text(&member["metadata"]["id"]),
                phase: text(&status["phase"]),
                termination: text(&status["termination"]["reason"]),
                oracle: oracle.map_or("none".into(), |p| text(&p["status"])),
                simulated_seconds: status["simulatedTimeNs"].as_f64().unwrap_or(0.0) / 1e9,
                verdict: oracle.map_or("-".into(), |p| text(&p["example"]["details"]["verdict"])),
                failing: properties
                    .iter()
                    .filter(|p| p["status"].as_str() != Some("passed"))
                    .map(|p| text(&p["id"]))
                    .collect(),
            }
        })
        .collect()
}

pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    let mut tally: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for row in rows {
        *tally
            .entry((&row.phase, &row.termination, &row.oracle))
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
    for ((phase, termination, oracle), count) in tally {
        out.push_str(&format!("{count} {phase} {termination} {oracle}\n"));
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
