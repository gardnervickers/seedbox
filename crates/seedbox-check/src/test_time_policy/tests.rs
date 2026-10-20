use super::*;
fn findings(code: &str) -> Vec<Finding> {
    source(Path::new("src/lib.rs"), code, false).unwrap()
}

#[test]
fn file_level_test_and_test_support_gates_mark_helpers_but_not_negated_gates() {
    for gate in ["test", "feature=\"test-support\""] {
        assert_eq!(
            findings(&format!(
                "#![cfg({gate})]\nfn helper(){{std::thread::sleep(delay);}}"
            ))
            .len(),
            1
        );
    }
    assert!(findings("#![cfg(not(test))]\nfn production(){std::thread::sleep(delay);}").is_empty());
}
#[test]
fn chained_aliases_and_aliased_globs_resolve_without_source_order_assumptions() {
    for code in [
        "use std::thread as host; use host::sleep as pause; #[test] fn f(){pause(delay);}",
        "use host::sleep as pause; use std::thread as host; #[test] fn f(){pause(delay);}",
        "use host::*; use std::thread as host; #[test] fn f(){sleep(delay);}",
        "use std::thread::sleep as pause; #[cfg(test)] mod tests {use super::*; fn helper(){pause(delay);}}",
        "use std::thread::sleep as pause; #[cfg(test)] mod tests {use super::pause; fn helper(){pause(delay);}}",
        "use std::thread as host; #[cfg(test)] mod tests {use super::host::sleep as pause; fn helper(){pause(delay);}}",
    ] { assert_eq!(findings(code).len(),1,"{code}"); }
    assert!(findings("use a as b; use b as a; #[test] fn f(){a::unrelated();}").is_empty());
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
    let code = r#"mod outer { #[cfg(test)] mod checks { fn helper() { let _ = std::thread::sleep(delay); } } }
#[test] fn check() { std::thread::sleep(std::time::Duration::ZERO); }
"#;
    assert_eq!(findings(code).len(), 2);
}
#[test]
fn direct_cfg_test_function_and_impl_method_are_scanned() {
    assert_eq!(findings("#[cfg(test)] fn f(){std::thread::sleep(delay);} struct X; impl X { #[cfg(test)] fn g(){std::thread::sleep(Duration::ZERO);} }").len(),2);
}
#[test]
fn negated_test_gate_does_not_mark_production_code_as_test_only() {
    assert!(
        findings("#[cfg(not(test))] mod actual { fn f(){std::thread::sleep(delay);} }").is_empty()
    );
}
#[test]
fn test_support_is_a_test_scope_without_a_test_named_file() {
    assert_eq!(
        findings("#[cfg(feature=\"test-support\")] mod helper{fn f(){std::thread::sleep(delay);}}")
            .len(),
        1
    );
}
#[test]
fn scopes_do_not_leak_a_nested_import_alias_outward() {
    let code="mod a { use std::thread::sleep as pause; #[test] fn f(){pause(Duration::ZERO);} } mod b { #[test] fn f(){pause();} }";
    assert_eq!(findings(code).len(), 1);
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
                "fn helper(){std::thread::sleep(delay);}",
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
            "fn hidden(){std::thread::sleep(delay);}",
        ),
        (
            "crates/b/src/main.rs",
            "#[cfg(test)] fn check(){std::thread::sleep(delay);}",
        ),
        (
            "crates/c/tests/case.rs",
            "fn helper(){std::thread::sleep(delay);}",
        ),
        ("crates/a/target/generated.rs", "not Rust"),
        (
            "crates/a/src/production.rs",
            "fn run(){std::thread::sleep(delay);}",
        ),
    ] {
        let file = root.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, code).unwrap();
    }
    assert_eq!(check(root).unwrap().findings.len(), 3);
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
fn cfg_test_trait_methods_are_not_missed() {
    assert_eq!(
        findings("trait Example {#[cfg(test)] fn probe(){std::thread::sleep(delay);}} ").len(),
        1
    );
}
#[test]
fn file_inner_gates_propagate_into_external_descendants() {
    for gate in ["test", "feature=\"test-support\""] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (name, code) in [
            ("crates/a/src/lib.rs".to_owned(), "mod helper;".to_owned()),
            (
                "crates/a/src/helper.rs".to_owned(),
                format!("#![cfg({gate})]\nfn helper(){{std::thread::sleep(delay);}} mod detail;"),
            ),
            (
                "crates/a/src/helper/detail.rs".to_owned(),
                "fn detail(){std::thread::sleep(delay);}".to_owned(),
            ),
        ] {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, code).unwrap();
        }
        assert_eq!(check(root).unwrap().findings.len(), 2);
    }
}
#[test]
fn external_path_modules_inherit_chained_parent_imports_through_super() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name,code) in [
        ("crates/a/src/lib.rs", "use std::thread as host; use host::sleep as pause; #[cfg(test)] #[path=\"check_body.rs\"] mod checks;"),
        ("crates/a/src/check_body.rs", "use super::*; fn helper(){pause(delay);} mod deep;"),
        ("crates/a/src/check_body/deep.rs", "use super::pause as again; fn helper(){again(delay);}"),
    ] {let file=root.join(name);fs::create_dir_all(file.parent().unwrap()).unwrap();fs::write(file,code).unwrap();}
    assert_eq!(check(root).unwrap().findings.len(), 2);
}
#[test]
fn shared_external_source_cannot_lose_a_known_time_import_from_another_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, code) in [
        ("crates/a/src/lib.rs", "mod a;mod b;"),
        (
            "crates/a/src/a.rs",
            "use std::thread::sleep as pause;#[cfg(test)] #[path=\"shared.rs\"]mod checks;",
        ),
        (
            "crates/a/src/b.rs",
            "use crate::ordinary::pause;#[cfg(test)] #[path=\"shared.rs\"]mod checks;",
        ),
        (
            "crates/a/src/shared.rs",
            "use super::pause;fn helper(){pause(delay);}",
        ),
    ] {
        let file = root.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, code).unwrap();
    }
    assert_eq!(check(root).unwrap().findings.len(), 1);
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
    assert_eq!(check(root).unwrap().findings.len(), 1);
}

#[test]
fn direct_async_native_and_aliased_sleep_calls_are_rejected() {
    for call in [
        "std::thread::sleep(delay)",
        "(std::thread::sleep)(delay)",
        "((tokio::time::sleep))(delay)",
        "std::thread::sleep_until(deadline)",
        "tokio::time::sleep(delay)",
        "tokio::time::sleep_until(deadline)",
        "async_std::task::sleep(delay)",
        "libc::usleep(10)",
        "libc::nanosleep(ptr,ptr)",
        "libc::clock_nanosleep(clock,0,ptr,ptr)",
    ] {
        assert_eq!(
            findings(&format!("#[test] fn f(){{{call};}}")).len(),
            1,
            "{call}"
        );
    }
    for code in [
        "use tokio::time::sleep as pause; #[tokio::test] async fn f(){pause(delay).await;}",
        "#[test] fn f(){use std::thread::sleep as pause; pause(delay);}",
        "use tokio::time::{self as timer}; #[test] fn f(){timer::sleep(delay);}",
        "use std::thread::{self,JoinHandle}; #[test] fn f(){thread::sleep(delay);}",
        "use std::{thread::{self as host}}; #[test] fn f(){host::sleep(delay);}",
        "use tokio::time::*; #[test] fn f(){sleep(delay);}",
        "use async_std::task::sleep as pause; #[test] fn f(){pause(delay);}",
        "use libc::nanosleep as pause; #[test] fn f(){pause(ptr,ptr);}",
        "use libc::*; #[test] fn f(){usleep(10);}",
    ] {
        assert_eq!(findings(code).len(), 1, "{code}");
    }
}

#[test]
fn clocks_timeouts_and_signal_waits_are_not_policed() {
    let code = r#"#[test] fn f(){
        let before=std::time::Instant::now();
        let now=std::time::SystemTime::now();
        let logical=tokio::time::Instant::now();
        let private=seedbox_engine_protocol::host_clock::Instant::now();
        assert!(before.elapsed()>limit);
        now.duration_since(before);
        now.checked_duration_since(before);
        now.saturating_duration_since(before);
        tokio::time::timeout(limit,work());
        tokio::time::timeout_at(deadline,work());
        receiver.recv_timeout(limit);
        condvar.wait_timeout(lock,limit);
        condvar.wait_timeout_while(lock,limit,predicate);
        stream.set_read_timeout(Some(limit));
        stream.set_write_timeout(Some(limit));
        Socket::connect_timeout(&addr,limit);
        std::thread::park_timeout(limit);
        libc::poll(ptr,1,30000);
        libc::ppoll(ptr,1,deadline,mask);
        libc::select(1,ptr,ptr,ptr,deadline);
        libc::pselect(1,ptr,ptr,ptr,deadline,mask);
        guard.recv(&receiver);
    }"#;
    assert!(findings(code).is_empty());
}

#[test]
fn nested_macro_calls_closures_and_timeout_bodies_are_scanned() {
    for body in [
        "assert!(std::thread::sleep(delay));",
        "std::thread::spawn(||std::thread::sleep(delay));",
        "loop {std::thread::sleep(delay);}",
        "tokio::time::timeout(limit,async{tokio::time::sleep(delay).await;});",
        "tokio::select!{_=work()=>{std::thread::sleep(delay);}}",
        "tokio::select!{_=tokio::time::sleep(delay)=>{},_=work()=>{}}",
        "custom!{outer!(std::thread::sleep(delay))}",
        "use tokio::time::sleep as pause; tokio::select!{_=pause(delay)=>{}}",
    ] {
        assert_eq!(
            findings(&format!("#[test] fn f(){{{body}}}")).len(),
            1,
            "{body}"
        );
    }
    assert!(findings(r####"#[test] fn f(){assert_eq!("std::thread::sleep(delay)", r###"tokio::time::sleep(delay)"###);}"####).is_empty());
}

#[test]
fn reports_each_sleep_location_and_displays_a_useful_diagnostic() {
    let out = findings(
        "#[test] fn f(){
std::thread::sleep(delay); std::thread::sleep(delay);
}",
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].line, 2);
    assert_eq!(
        out[0].to_string(),
        "src/lib.rs:2: sleep call std::thread::sleep in test code; wait on a signal instead"
    );
}

#[test]
fn explicit_test_context_and_parse_errors_are_preserved() {
    assert_eq!(
        source(
            Path::new("src/helper.rs"),
            "fn f(){std::thread::sleep(delay);}",
            true
        )
        .unwrap()
        .len(),
        1
    );
    assert!(source(Path::new("tests/broken.rs"), "not Rust", false)
        .unwrap_err()
        .contains("tests/broken.rs"));
}

#[test]
fn controlled_timers_and_unrelated_sleep_named_functions_are_not_host_sleeps() {
    for code in [
        "#[test] fn f(){activity::Controlled::sleep_until(&timer,deadline);}",
        "use activity::Controlled as Timer; #[test] fn f(){Timer::sleep_until(&timer,deadline);}",
        "use activity::Controlled; #[test] fn f(){Controlled::sleep_until(&timer,deadline);}",
        "#[test] fn f(){timer.sleep_until(deadline);timer.sleep(delay);}",
        "fn sleep(delay:u64){} #[test] fn f(){sleep(42);}",
        "#[test] fn f(){assert!(timer.sleep_until(deadline));}",
        "#[test] fn f(){tokio::select!{_=activity::Controlled::sleep_until(&timer,deadline)=>{}}}",
        "use tokio::time::sleep as pause; #[test] fn f(){assert!(timer.pause(delay));}",
    ] {
        assert!(findings(code).is_empty(), "{code}");
    }
}

#[test]
fn dr_syscall_subject_is_one_bounded_documented_call_not_a_pacing_exemption() {
    let path = Path::new("vendor/dynamorio/seedbox-tests/dynamorio/unix/subject.c");
    let marker = "/* seedbox-syscall-subject: nanosleep; bound-ms=200; invariant=observed blocked-entry EINTR and remainder */";
    assert!(dr_c_findings(
        path,
        &format!("{marker}\nlong r = syscall(SYS_nanosleep, &req, &rem);")
    )
    .is_empty());
    assert_eq!(
        dr_c_findings(
            path,
            &format!("{marker} nanosleep(&req,&rem); nanosleep(&req,&rem);")
        )
        .len(),
        1
    );
    assert!(dr_c_findings(
        path,
        &format!("{marker} syscall(__NR_nanosleep,&req,&rem);")
    )
    .is_empty());
    assert_eq!(
        dr_c_findings(path, "syscall(__NR_nanosleep,&req,&rem);").len(),
        1
    );
    for invalid in [
        "/* seedbox-syscall-subject: nanosleep; bound-ms=0; invariant=EINTR */",
        "/* seedbox-syscall-subject: nanosleep; bound-ms=200; invariant= */",
        "/* seedbox-syscall-subject: clock_nanosleep; bound-ms=200; invariant=EINTR */",
        "/* allow-sleep: readiness */",
    ] {
        assert_eq!(
            dr_c_findings(path, &format!("{invalid} nanosleep(&req,&rem);")).len(),
            1
        );
    }
    assert_eq!(
        dr_c_findings(path, &format!("{marker} usleep(10);")).len(),
        1
    );
    assert_eq!(
        dr_c_findings(
            path,
            "sleep(1); syscall(SYS_clock_nanosleep,clock,flags,&req,&rem);"
        )
        .len(),
        2
    );
    assert!(dr_c_findings(path, "/* nanosleep(0,0); */ puts(\"nanosleep(0,0);\");").is_empty());
    // A C subject marker never exempts Rust scheduling helpers.
    assert_eq!(
        source(
            Path::new("tests/a.rs"),
            &format!("{marker} fn f(){{std::thread::sleep(delay);}}"),
            true
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn fork_suite_helpers_are_scanned_without_scanning_upstream_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, code) in [
        ("vendor/dynamorio/seedbox-tests/dynamorio.rs", "mod helper;"),
        (
            "vendor/dynamorio/seedbox-tests/helper.rs",
            "fn f(){std::thread::sleep(delay);}",
        ),
        (
            "vendor/dynamorio/seedbox-tests/subject.c",
            "nanosleep(&req,&rem);",
        ),
        ("vendor/dynamorio/suite/tests/subject.c", "sleep(1);"),
        (
            "vendor/dynamorio/suite/tests/helper.rs",
            "fn f(){std::thread::sleep(delay);}",
        ),
    ] {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, code).unwrap();
    }
    let report = check(root).unwrap();
    assert_eq!(report.findings.len(), 2);
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.path.starts_with("vendor/dynamorio/seedbox-tests")));
}

#[test]
fn missing_fork_suite_in_a_seedbox_checkout_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("crates/seedbox-machine/Cargo.toml");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(manifest, "").unwrap();
    assert!(check(dir.path())
        .unwrap_err()
        .contains("git submodule update --init --recursive vendor/dynamorio"));
}
