use super::*;
fn findings(code: &str) -> Vec<Finding> {
    source(Path::new("src/lib.rs"), code, false).unwrap()
}

#[test]
fn production_time_is_not_a_test_oracle() {
    assert!(findings(
        "fn serve() { let start=std::time::Instant::now(); std::thread::sleep(start.elapsed()); }"
    )
    .is_empty());
}
#[test]
fn detects_test_functions_and_cfg_test_items_at_any_depth() {
    let code = r#"mod outer { #[cfg(test)] mod checks { fn helper() { let _ = std::time::Instant::now(); } } }
#[test] fn check() { std::thread::sleep(std::time::Duration::ZERO); }
"#;
    assert_eq!(findings(code).len(), 2);
}
#[test]
fn direct_cfg_test_function_and_impl_method_are_scanned() {
    assert_eq!(findings("#[cfg(test)] fn f(){std::time::Instant::now();} struct X; impl X { #[cfg(test)] fn g(){std::thread::sleep(Duration::ZERO);} }").len(),2);
}
#[test]
fn negated_test_gate_does_not_mark_production_code_as_test_only() {
    assert!(
        findings("#[cfg(not(test))] mod actual { fn f(){std::time::Instant::now();} }").is_empty()
    );
}
#[test]
fn test_support_is_a_test_scope_without_a_test_named_file() {
    assert_eq!(
        findings("#[cfg(feature=\"test-support\")] mod helper{fn f(){std::time::Instant::now();}}")
            .len(),
        1
    );
}
#[test]
fn renamed_imports_and_namespace_aliases_cannot_hide_real_time() {
    for code in [
        "use std::time::Instant as Wall; #[test] fn f(){Wall::now();}",
        "use std::thread::sleep as pause; #[test] fn f(){pause(Duration::ZERO);}",
        "use std::time as wall; #[test] fn f(){wall::Instant::now();}",
        "#[test] fn f(){use tokio::time::timeout as bounded; bounded(Duration::ZERO, work());}",
    ] {
        assert!(!findings(code).is_empty(), "{code}");
    }
}
#[test]
fn grouped_self_and_renamed_self_imports_resolve_the_namespace() {
    for code in [
        "use std::thread::{self,JoinHandle}; #[test] fn f(){thread::sleep(Duration::ZERO);}",
        "use std::{thread::{self as host},time::{self as wall}}; #[test] fn f(){host::sleep(wall::Duration::ZERO);}",
        "use tokio::time::{self as timer}; #[test] fn f(){timer::timeout(limit,work());}",
    ] { assert_eq!(findings(code).len(),1,"{code}"); }
}
#[test]
fn perturbation_exemption_does_not_hide_a_following_ordering_delay() {
    let out=findings("#[test] fn f(){\n// test-time: allow(host-timing-perturbation) -- compare exact outputs across this injected host jitter\nstd::thread::sleep(jitter);\nstd::thread::sleep(ordering_delay);\n}");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].line, 4);
}
#[test]
fn a_timeout_guard_does_not_grandfather_its_async_body() {
    let out=findings("#[test] fn f(){\n// test-time: allow(hang-guard) -- guard only the positive future completion\ntokio::time::timeout(limit,async{std::thread::sleep(ordering_delay);}).await;\n}");
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].detail.contains("std::thread::sleep"));
}
#[test]
fn outer_annotations_cannot_blanket_closures_loops_or_select_bodies() {
    for body in [
        "std::thread::spawn(||std::thread::sleep(delay));",
        "loop {std::thread::sleep(delay);}",
        "tokio::select!{ _=work()=>{std::thread::sleep(delay);} }",
    ] {
        let out=findings(&format!("#[test] fn f(){{\n// test-time: allow(hang-guard) -- not a body-wide permission\n{body}\n}}"));
        assert_eq!(out.len(), 2, "{body}: {out:?}");
        assert!(out.iter().any(|f| f.detail.contains("unused")));
        assert!(out.iter().any(|f| f.detail.contains("raw host-time")));
    }
}
#[test]
fn select_body_primitives_can_have_their_own_narrow_annotation() {
    assert!(findings("#[test] fn f(){tokio::select!{_=work()=>{\n// test-time: allow(hang-guard) -- poll only the positive cancellation predicate\nstd::thread::sleep(poll_interval);\n}}}").is_empty());
}
#[test]
fn scopes_do_not_leak_a_nested_import_alias_outward() {
    let code="mod a { use std::thread::sleep as pause; #[test] fn f(){pause(Duration::ZERO);} } mod b { #[test] fn f(){pause();} }";
    assert_eq!(findings(code).len(), 1);
}
#[test]
fn elapsed_inside_assert_and_select_macro_are_inspected_without_matching_strings() {
    assert_eq!(
        findings("#[test] fn f(){ assert!(start.elapsed()<limit); }").len(),
        1
    );
    assert_eq!(findings("#[test] fn f(){ tokio::select! { _ = tokio::time::sleep(limit) => {}, _ = work() => { assert!(s.elapsed()<limit); } } }").len(),2);
    assert!(findings(
        "#[test] fn f(){ assert_eq!(\"start.elapsed()\", \"std::time::Instant::now()\"); }"
    )
    .is_empty());
}
#[test]
fn positive_guard_calls_need_no_raw_boundary_annotation() {
    assert!(findings("#[test] fn f(){let guard=HangGuard::new(\"peer must reply\",LIMIT);guard.recv(&receiver).unwrap();}").is_empty());
}
#[test]
fn all_reasoned_exemptions_apply_only_to_the_following_statement() {
    for kind in KINDS {
        let code=format!("#[test] fn f(){{\n// test-time: allow({kind}) -- this exact boundary has an independent semantic assertion\nlet _ = std::time::Instant::now();\nstd::thread::sleep(Duration::ZERO);\n}}");
        let out = findings(&code);
        assert_eq!(out.len(), 1, "{kind}: {out:?}");
        assert_eq!(out[0].line, 4);
    }
}
#[test]
fn exemption_on_macro_covers_its_nested_tokens() {
    assert!(findings("#[test] fn f(){\n// test-time: allow(host-time-subject) -- exercise wall-clock accounting itself\nassert!(start.elapsed()>limit);\n}").is_empty());
}
#[test]
fn invalid_empty_stale_and_blanket_exemptions_are_errors() {
    for annotation in [
        "// test-time: allow(legacy) -- no",
        "// test-time: allow(hang-guard) -- ",
        "// test-time: allow(hang-guard): missing delimiter",
    ] {
        let code = format!("#[test] fn f(){{\n{annotation}\nlet x=1;\n}}");
        assert!(!findings(&code).is_empty());
    }
    assert!(!findings("// test-time: allow(hang-guard) -- blanket module exemption\n#[cfg(test)] mod checks {fn f(){std::time::Instant::now();}}").is_empty());
    assert!(!findings(
        "#[test] fn f(){\n// test-time: allow(hang-guard) -- stale exemption\nlet x=1;\n}"
    )
    .is_empty());
}
#[test]
fn example_exemption_inside_raw_string_is_not_an_actual_exemption() {
    let code="#[test] fn f(){let example=r###\"\n// test-time: allow(hang-guard) -- this is only fixture data\nlet _ = std::time::Instant::now();\n\"###;}";
    assert!(findings(code).is_empty());
}
#[test]
fn integration_and_helper_sources_are_scanned_without_cfg_attributes() {
    for path in [
        "crates/x/tests/integration.rs",
        "crates/x/tests/support/peer.rs",
        "crates/x/src/test_support/fixture.rs",
        "crates/x/src/thing_tests.rs",
    ] {
        assert_eq!(
            source(
                Path::new(path),
                "fn helper(){std::time::Instant::now();}",
                false
            )
            .unwrap()
            .len(),
            1,
            "{path}"
        );
    }
}
#[test]
fn check_includes_standalone_workspaces_and_external_gated_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, code) in [
        ("crates/a/src/lib.rs", "#[cfg(test)] mod hidden;"),
        (
            "crates/a/src/hidden.rs",
            "fn hidden(){std::time::Instant::now();}",
        ),
        (
            "crates/seedbox-vmm/src/main.rs",
            "#[cfg(test)] fn check(){std::time::Instant::now();}",
        ),
        (
            "crates/seedbox-arm64/tests/case.rs",
            "fn helper(){std::time::Instant::now();}",
        ),
        ("crates/a/target/generated.rs", "not Rust"),
        (
            "crates/a/src/production.rs",
            "fn run(){std::time::Instant::now();}",
        ),
    ] {
        let file = root.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, code).unwrap();
    }
    assert_eq!(check_with_pending(root, &[]).unwrap().findings.len(), 3);
}
#[test]
fn qualified_calls_and_glob_imports_cannot_evade_the_policy() {
    for code in [
        "#[test] fn f(){std::sync::mpsc::Receiver::recv_timeout(&receiver, limit);}",
        "use std::thread::*; #[test] fn f(){sleep(limit);}",
        "#[test] fn f(){libc::poll(ptr,1,100);}",
        "#[test] fn f(){client.timeout(limit);}",
        "#[test] fn f(){now.saturating_duration_since(before);}",
    ] {
        assert!(!findings(code).is_empty(), "{code}");
    }
}
#[test]
fn exemption_cannot_expand_to_later_statements_on_the_same_line() {
    let code="#[test] fn f(){\n// test-time: allow(measurement) -- only the first clock sample is a reported metric\nlet first=std::time::Instant::now(); let second=std::time::Instant::now();\n}";
    assert_eq!(findings(code).len(), 1);
}
#[test]
fn explicit_pending_debt_requires_exact_registered_bead_site_owner_and_operations() {
    const SITE: PendingSite = PendingSite {
        bead: "bd-dd9",
        id: "known-clock",
        path: "src/lib.rs",
        owner: "f",
        operations: &["function:std::time::Instant::now"],
    };
    let good="#[test] fn f(){\n// test-time: pending(bd-dd9, known-clock) -- tracked native progress conversion\nlet _ = std::time::Instant::now();\n}";
    let report = source_with_pending(
        Path::new("src/lib.rs"),
        good,
        false,
        std::slice::from_ref(&SITE),
    )
    .unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.pending_sites, ["known-clock"]);
    for bad in [
        good.replace("bd-dd9", "bd-other"),
        good.replace("known-clock", "new-site"),
        good.replace("fn f", "fn other"),
        good.replace("Instant::now()", "Instant::now().elapsed()"),
    ] {
        assert!(
            !source_with_pending(
                Path::new("src/lib.rs"),
                &bad,
                false,
                std::slice::from_ref(&SITE)
            )
            .unwrap()
            .findings
            .is_empty(),
            "{bad}"
        );
    }
    assert!(
        !source_with_pending(Path::new("tests/other.rs"), good, false, &[SITE])
            .unwrap()
            .findings
            .is_empty()
    );
}
#[test]
fn pending_sites_cannot_be_duplicated_or_left_in_the_registry_after_removal() {
    const SITE: PendingSite = PendingSite {
        bead: "bd-dd9",
        id: "known-clock",
        path: "crates/a/tests/case.rs",
        owner: "f",
        operations: &["function:std::time::Instant::now"],
    };
    let code="fn f(){\n// test-time: pending(bd-dd9, known-clock) -- old tracked clock\nlet _ = std::time::Instant::now();\n// test-time: pending(bd-dd9, known-clock) -- copied marker must not create new permission\nlet _ = std::time::Instant::now();\n}";
    assert!(!source_with_pending(
        Path::new(SITE.path),
        code,
        true,
        std::slice::from_ref(&SITE)
    )
    .unwrap()
    .findings
    .is_empty());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(SITE.path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "fn f(){}").unwrap();
    let report = check_with_pending(dir.path(), &[SITE]).unwrap();
    assert!(report.pending_sites.is_empty());
    assert!(report.findings.iter().any(|f| f.detail.contains("stale")));
}
#[test]
fn pending_can_target_one_field_expression_without_exempting_other_fields() {
    const SITE: PendingSite = PendingSite {
        bead: "bd-dd9",
        id: "session-clock",
        path: "tests/fixture.rs",
        owner: "Session::new",
        operations: &["function:std::time::Instant::now"],
    };
    let code="struct Session{} impl Session {fn new(){Self{\n// test-time: pending(bd-dd9, session-clock) -- tracked old phase clock\nstarted:std::time::Instant::now(),\nother:std::time::Instant::now(),\n};}}";
    let report = source_with_pending(Path::new(SITE.path), code, true, &[SITE]).unwrap();
    assert_eq!(report.pending_sites, ["session-clock"]);
    assert_eq!(report.findings.len(), 1);
}
#[test]
fn ordinary_timeout_and_sleep_data_are_not_host_time_operations() {
    assert!(findings(r#"#[test] fn f(){ let timeout=42; let sleep=false; assert_eq!(timeout,42); values.push(sleep); let _=json!({"timeout":timeout,"sleep":sleep}); }"#).is_empty());
    assert!(
        findings("fn timeout(n:u64)->u64{n} #[test] fn f(){assert_eq!(timeout(42),42);}")
            .is_empty()
    );
}
#[test]
fn known_time_globs_resolve_real_primitives_without_guessing_local_names() {
    for code in [
        "use std::thread::*; #[test] fn f(){sleep(Duration::ZERO);}",
        "use tokio::time::*; #[test] fn f(){timeout(limit,work());}",
        "use std::time::*; #[test] fn f(){SystemTime::now();}",
    ] {
        assert_eq!(findings(code).len(), 1, "{code}");
    }
}
#[test]
fn cfg_test_trait_methods_are_not_missed() {
    assert_eq!(
        findings("trait Example {#[cfg(test)] fn probe(){std::time::Instant::now();}} ").len(),
        1
    );
}
#[test]
fn external_path_attribute_and_nested_test_gate_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, code) in [
        (
            "crates/a/src/lib.rs",
            "#[cfg(feature=\"test-support\")] #[path=\"other.rs\"] mod fixtures;",
        ),
        ("crates/a/src/other.rs", "mod peer;"),
        (
            "crates/a/src/other/peer.rs",
            "fn helper(){std::thread::sleep(Duration::ZERO);}",
        ),
    ] {
        let file = root.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, code).unwrap();
    }
    assert_eq!(check_with_pending(root, &[]).unwrap().findings.len(), 1);
}
