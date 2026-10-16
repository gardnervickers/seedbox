//! Exact pre-existing debt, owned by bd-d98. There is deliberately no refresh mode.
use super::{Finding, Report, SiteKey};
use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

pub const FILE: &str = "crates/seedbox-check/test-time-baseline.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    path: PathBuf,
    site: SiteKey,
}

// Keep bindings, arguments and assertion messages, not source line numbers or
// whitespace. Nested bodies have their own smallest statements and findings;
// changing unrelated code inside an async body must not refresh its outer guard.
fn tokens(stream: TokenStream) -> String {
    stream
        .into_iter()
        .map(|token| match token {
            TokenTree::Group(group) => match group.delimiter() {
                Delimiter::Brace => "{ <body> }".into(),
                Delimiter::Parenthesis => format!("( {} )", tokens(group.stream())),
                Delimiter::Bracket => format!("[ {} ]", tokens(group.stream())),
                Delimiter::None => tokens(group.stream()),
            },
            token => token.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub(super) fn context(source: &str, span: Span) -> String {
    let text = source
        .get(span.byte_range())
        .expect("parser span belongs to its source");
    tokens(
        text.parse()
            .expect("AST span contains balanced Rust tokens"),
    )
}
fn text(root: &Path) -> Result<String, String> {
    match fs::read_to_string(root.join(FILE)) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("{FILE}: {error}")),
    }
}
pub(super) fn registered_count(root: &Path) -> Result<usize, String> {
    Ok(text(root)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}
fn parse(text: &str) -> Result<Vec<(usize, Entry)>, String> {
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let bad = |why: &str| format!("{FILE}:{number}: {why}");
        let value: Value = serde_json::from_str(line).map_err(|error| bad(&error.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| bad("expected a baseline site object"))?;
        if object.len() != 5 {
            return Err(bad(
                "expected exactly path, owner, operation, context and occurrence",
            ));
        }
        let field = |name: &str| -> Result<String, String> {
            object
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| bad(&format!("missing/nonempty string field {name}")))
        };
        let path = PathBuf::from(field("path")?);
        if path.is_absolute()
            || !path.starts_with("crates")
            || path.extension().is_none_or(|ext| ext != "rs")
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(bad(
                "baseline path must be a repository-relative Rust source under crates",
            ));
        }
        let operation = field("operation")?;
        if !operation.starts_with("function:") && !operation.starts_with("method:") {
            return Err(bad(
                "baseline operation must identify one raw function or method",
            ));
        }
        let occurrence = object
            .get("occurrence")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| bad("occurrence must be a positive integer"))?;
        let entry = Entry {
            path,
            site: SiteKey {
                owner: field("owner")?,
                operation,
                context: field("context")?,
                occurrence,
            },
        };
        if entries.insert(entry, number).is_some() {
            return Err(bad("duplicate baseline site"));
        }
    }
    Ok(entries
        .into_iter()
        .map(|(entry, line)| (line, entry))
        .collect())
}
fn apply_text(mut report: Report, contents: &str) -> Result<Report, String> {
    let mut entries: BTreeMap<_, _> = parse(contents)?
        .into_iter()
        .map(|(line, entry)| (entry, (line, false)))
        .collect();
    report.baseline_registered = entries.len();
    report.findings.retain(|finding| {
        let Some(site) = &finding.site else {
            return true;
        };
        let entry = Entry {
            path: finding.path.clone(),
            site: site.clone(),
        };
        if let Some((_, used)) = entries.get_mut(&entry) {
            // A repeated finding may never spend one baseline row twice.
            if *used {
                return true;
            }
            *used = true;
            report.baseline_matched += 1;
            false
        } else {
            true
        }
    });
    for (entry, (line, used)) in entries {
        if !used {
            report.findings.push(Finding {
                path: FILE.into(), line,
                detail: format!("stale baseline site: {} [{}; {}; occurrence={}]; remove this row when its raw site disappears", entry.path.display(), entry.site.owner, entry.site.operation, entry.site.occurrence),
                site: None,
            });
        }
    }
    Ok(report)
}
pub(super) fn apply(root: &Path, report: Report) -> Result<Report, String> {
    apply_text(report, &text(root)?)
}
#[cfg(test)]
fn record(finding: &Finding) -> String {
    let site = finding
        .site
        .as_ref()
        .expect("only raw findings can be baselined");
    json!({"path":finding.path.to_str().unwrap(),"owner":site.owner,"operation":site.operation,
        "context":site.context,"occurrence":site.occurrence})
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scan(code: &str) -> Report {
        super::super::source_with_pending(Path::new("crates/a/src/lib.rs"), code, false, &[])
            .unwrap()
    }
    #[test]
    fn repository_ratchet_accepts_exact_current_debt_without_masking_errors() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let report = super::super::check(&root).unwrap();
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
        assert_eq!(report.baseline_registered, registered_count(&root).unwrap());
        assert_eq!(report.baseline_matched, report.baseline_registered);
        assert_eq!(
            report.pending_sites.len(),
            super::super::registered_pending_count()
        );
        eprintln!(
            "ratchet: baseline {}/{}; pending {}/{}",
            report.baseline_matched,
            report.baseline_registered,
            report.pending_sites.len(),
            super::super::registered_pending_count()
        );
    }
    #[test]
    fn existing_site_is_accepted_but_a_new_site_is_not() {
        let original = scan("#[test] fn f(){let start=std::time::Instant::now();}");
        let row = record(&original.findings[0]);
        let accepted = apply_text(original, &row).unwrap();
        assert_eq!(
            (accepted.baseline_registered, accepted.baseline_matched),
            (1, 1)
        );
        assert!(accepted.findings.is_empty());
        let expanded =
            scan("#[test] fn f(){let start=std::time::Instant::now();std::thread::sleep(delay);}");
        let rejected = apply_text(expanded, &row).unwrap();
        assert_eq!(rejected.findings.len(), 1);
        assert!(rejected.findings[0].detail.contains("std::thread::sleep"));
    }
    #[test]
    fn removal_requires_deleting_the_baseline_row_and_can_shrink_to_zero() {
        let original = scan("#[test] fn f(){std::thread::sleep(delay);}");
        let row = record(&original.findings[0]);
        let stale = apply_text(scan("#[test] fn f(){}"), &row).unwrap();
        assert_eq!(stale.findings.len(), 1);
        assert!(stale.findings[0].detail.contains("stale baseline"));
        assert!(apply_text(scan("#[test] fn f(){}"), "")
            .unwrap()
            .findings
            .is_empty());
    }
    #[test]
    fn identity_survives_formatting_and_unrelated_line_insertions() {
        let original = scan("#[test] fn f(){let start=std::time::Instant::now();}");
        let row = record(&original.findings[0]);
        let changed=scan("// unrelated file header\n\n#[test]\nfn f() {\n    let start = std::time::Instant::now();\n}\n");
        assert!(apply_text(changed, &row).unwrap().findings.is_empty());
    }
    #[test]
    fn bindings_arguments_owner_and_path_are_part_of_the_exact_site() {
        let original = scan("#[test] fn f(){let start=std::time::Instant::now();}");
        let row = record(&original.findings[0]);
        for changed in [
            "#[test] fn f(){let other=std::time::Instant::now();}",
            "#[test] fn g(){let start=std::time::Instant::now();}",
        ] {
            assert_eq!(apply_text(scan(changed), &row).unwrap().findings.len(), 2);
        }
        let mut moved = scan("#[test] fn f(){let start=std::time::Instant::now();}");
        moved.findings[0].path = "crates/b/src/lib.rs".into();
        assert_eq!(apply_text(moved, &row).unwrap().findings.len(), 2);
        let argument_row = record(&scan("#[test] fn f(){std::thread::sleep(delay);}").findings[0]);
        assert_eq!(
            apply_text(
                scan("#[test] fn f(){std::thread::sleep(other_delay);}"),
                &argument_row
            )
            .unwrap()
            .findings
            .len(),
            2
        );
    }
    #[test]
    fn duplicate_calls_on_one_line_need_distinct_baseline_rows() {
        let original = scan("#[test] fn f(){std::thread::sleep(delay);}");
        let row = record(&original.findings[0]);
        let expanded = scan("#[test] fn f(){std::thread::sleep(delay);std::thread::sleep(delay);}");
        assert_eq!(expanded.findings.len(), 2);
        let rejected = apply_text(expanded, &row).unwrap();
        assert_eq!(rejected.findings.len(), 1);
        assert_eq!(rejected.findings[0].site.as_ref().unwrap().occurrence, 2);
        assert!(parse(&format!("{row}\n{row}"))
            .unwrap_err()
            .contains("duplicate"));
    }
    #[test]
    fn baselined_outer_timeout_does_not_hide_new_code_inside_its_body() {
        let original = scan("#[test] fn f(){tokio::time::timeout(limit,async{}).await;}");
        let row = record(&original.findings[0]);
        let expanded = scan(
            "#[test] fn f(){tokio::time::timeout(limit,async{std::thread::sleep(delay);}).await;}",
        );
        let rejected = apply_text(expanded, &row).unwrap();
        assert_eq!(rejected.findings.len(), 1);
        assert!(rejected.findings[0].detail.contains("std::thread::sleep"));
    }
    #[test]
    fn malformed_or_expanded_baseline_records_fail_closed() {
        let original = scan("#[test] fn f(){std::thread::sleep(delay);}");
        let row = record(&original.findings[0]);
        for (field, value) in [
            ("path", json!("../outside.rs")),
            ("owner", json!("")),
            ("occurrence", json!(0)),
            ("extra", json!(true)),
        ] {
            let mut malformed: Value = serde_json::from_str(&row).unwrap();
            malformed[field] = value;
            assert!(parse(&malformed.to_string()).is_err());
        }
        assert!(parse("not JSON").is_err());
        assert_eq!(
            apply_text(Report::default(), &row).unwrap().findings.len(),
            1
        );
    }
    #[test]
    fn policy_errors_cannot_be_suppressed_as_baseline_debt() {
        let original = scan("#[test] fn f(){std::thread::sleep(delay);}");
        let row = record(&original.findings[0]);
        let invalid=scan("#[test] fn f(){\n// test-time: allow(unknown) -- invalid reason category\nstd::thread::sleep(delay);}");
        let expected: Vec<_> = invalid
            .findings
            .iter()
            .filter(|f| f.site.is_none())
            .cloned()
            .collect();
        assert!(!expected.is_empty());
        let rejected = apply_text(invalid, &row).unwrap();
        assert_eq!(rejected.findings, expected);
        assert_eq!(rejected.baseline_matched, 1);
    }
}
