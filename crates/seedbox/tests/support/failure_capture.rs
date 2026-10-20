//! Bounded observations of a failed test's still-live server. Never changes its verdict.
use seedbox_test::process::Guarded;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
};

pub const LIMIT: u64 = 8 * 1024 * 1024;

pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() as u64 > LIMIT {
        return Err(format!("{} exceeds diagnostic 8 MiB limit", path.display()));
    }
    Ok(bytes)
}

pub fn note(directory: &Path, message: &str) {
    eprintln!("failure capture: {message}");
    let result = fs::create_dir_all(directory).and_then(|()| {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("summary.txt"))?,
            "{message}"
        )
    });
    if let Err(error) = result {
        eprintln!("failure capture summary {}: {error}", directory.display());
    }
}

// Drain both pipes even after their retained prefix is full, so a long guest
// line cannot grow disk usage or deadlock the observer. Each file stays <=8 MiB.
fn drain(mut input: impl Read, path: &Path) -> Result<(), String> {
    let mut output = fs::File::create(path).map_err(|error| error.to_string())?;
    let kept = io::copy(&mut input.by_ref().take(LIMIT), &mut output)
        .map_err(|error| error.to_string())?;
    let omitted = io::copy(&mut input, &mut io::sink()).map_err(|error| error.to_string())?;
    if omitted != 0 {
        return Err(format!(
            "{}: retained {kept} bytes; omitted {omitted} bytes",
            path.display()
        ));
    }
    Ok(())
}

pub fn command(command: &mut Command, directory: &Path, label: &str) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .guarded_spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("missing observation stdout")?;
    let stderr = child.stderr.take().ok_or("missing observation stderr")?;
    let (out, err, status) = std::thread::scope(|scope| {
        let out = scope.spawn(|| drain(stdout, &directory.join(format!("{label}.stdout"))));
        let err = scope.spawn(|| drain(stderr, &directory.join(format!("{label}.stderr"))));
        let status = child.wait();
        (out.join(), err.join(), status)
    });
    let mut errors = Vec::new();
    for result in [out, err] {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(_) => errors.push("observation reader panicked".into()),
        }
    }
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => errors.push(format!("{label}: {status}")),
        Err(error) => errors.push(error.to_string()),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
