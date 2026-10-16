//! Oracle-only world control. Only the simulation's oracle machine has the
//! control socket; elsewhere these calls fail with `NotFound`.
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The oracle's control socket.
pub const SOCKET: &str = "/run/seedbox/control.sock";

/// Start no generated fault for `duration` of simulated time, so the system
/// can recover; generated faults already active still end on schedule.
/// Returns the simulated time, in nanoseconds, at which the quiet period ends.
pub fn quiet(duration: Duration) -> io::Result<u64> {
    let duration_ns = u64::try_from(duration.as_nanos())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "quiet duration too long"))?;
    let result = call(SOCKET, json!({"op": "quiet", "duration_ns": duration_ns}))?;
    result["quiet_until_ns"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid quiet result"))
}

/// One request on a fresh connection, returning its `result`.
fn call(path: &str, mut request: Value) -> io::Result<Value> {
    request["version"] = json!(1);
    request["id"] = json!(1);
    let mut stream = UnixStream::connect(path)?;
    let mut line = serde_json::to_vec(&request).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply: Value =
        serde_json::from_str(&reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if reply["ok"] != json!(true) {
        let message = reply["error"]["message"]
            .as_str()
            .unwrap_or("control request failed");
        return Err(io::Error::other(message.to_string()));
    }
    Ok(reply["result"].clone())
}
