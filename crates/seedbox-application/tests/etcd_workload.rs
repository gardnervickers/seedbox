//! Guest writer accounting is checked in Rust against the actual Go implementation.
use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn fixed_and_duration_writes_preserve_keys_deadlines_and_concurrency() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(
        root.join("writes.go"),
        include_str!("../../../examples/etcd/workload/writes.go"),
    )
    .unwrap();
    fs::write(root.join("main.go"), r#"
package main
import ("context"; "encoding/json"; "errors"; "os"; "sync"; "time")
func main() {
    scenario := os.Args[1]
    var mu sync.Mutex
    calls := map[string]int{}
    keys := []string{}
    entered := 0
    ready := make(chan struct{})
    duration, concurrency, writes := time.Second, 3, 4
    if scenario == "deadline" { duration, concurrency, writes = 50*time.Millisecond, 1, 2 }
    if scenario == "duration" { duration, concurrency, writes = 250*time.Millisecond, 1, 0 }
    if scenario == "concurrency" { writes = 1 }
    acknowledgments, failed := runWrites(duration, concurrency, writes, func(ctx context.Context, key, value string) (int64,error) {
        mu.Lock()
        calls[key]++
        keys = append(keys, key)
        entered++
        if scenario == "concurrency" && entered == concurrency { close(ready) }
        retry := calls[key] == 1 && key == "k/01/000001"
        mu.Unlock()
        if scenario == "concurrency" { select { case <-ready: case <-ctx.Done(): return 0,ctx.Err() } }
        if scenario == "deadline" { time.Sleep(100*time.Millisecond); return 0,errors.New("unavailable") }
        if scenario == "duration" || retry { return 0,errors.New("transient") }
        return 42,nil
    })
    receipts := []map[string]any{}
    for _, receipt := range acknowledgments { receipts=append(receipts,map[string]any{"key":receipt.key,"value":receipt.val,"revision":receipt.rev}) }
    json.NewEncoder(os.Stdout).Encode(map[string]any{"receipts":receipts,"failed":failed,"calls":calls,"keys":keys,"entered":entered})
}
"#).unwrap();
    let binary = root.join("writer");
    let built = Command::new("go")
        .args(["build", "-o"])
        .arg(&binary)
        .args(["main.go", "writes.go"])
        .current_dir(root)
        .env("GOTOOLCHAIN", "local")
        .env("GO111MODULE", "off")
        .env("GOCACHE", root.join("cache"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = |scenario| {
        let output = Command::new(&binary).arg(scenario).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let fixed = run("fixed");
    assert_eq!(fixed["receipts"].as_array().unwrap().len(), 12);
    assert_eq!(fixed["failed"], 1);
    for worker in 0..3 {
        for index in 0..4 {
            let key = format!("k/{worker:02}/{index:06}");
            assert_eq!(
                fixed["calls"][&key],
                if key == "k/01/000001" { 2 } else { 1 }
            );
        }
    }
    for receipt in fixed["receipts"].as_array().unwrap() {
        assert_eq!(receipt["revision"], 42);
        let index: u32 = receipt["key"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(receipt["value"], format!("v{index}"));
    }
    let incomplete = run("deadline");
    assert!(incomplete["receipts"].as_array().unwrap().is_empty());
    assert_eq!(incomplete["failed"], 1);
    let duration = run("duration");
    let keys = duration["keys"].as_array().unwrap();
    assert!(keys.len() >= 2);
    assert_eq!(duration["failed"], keys.len());
    for (index, key) in keys.iter().enumerate() {
        assert_eq!(key, &format!("k/00/{index:06}"));
    }
    let concurrent = run("concurrency");
    assert_eq!(concurrent["entered"], 3);
    assert_eq!(concurrent["receipts"].as_array().unwrap().len(), 3);
    assert_eq!(concurrent["failed"], 0);
}
