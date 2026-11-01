//! The [`World`] an oracle holds inside a Seedbox guest: the v1 guest
//! control RPC on `/run/seedbox/control.sock`.
//!
//! The RPC is one JSON object per line, at most [`MAX_FRAME`] bytes with its
//! newline, numbered from 1 per connection, as `python/seedbox/control.py`
//! speaks it. Each call blocks until the world confirms its effect; there
//! are no retries, and a transport or protocol failure closes the
//! connection. Only the calls [`World`] needs are here.
use crate::{control::Action, Error, World};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

/// Largest request or response line, newline included.
pub const MAX_FRAME: usize = 16_384;
/// Where an oracle finds its controller socket.
pub const DEFAULT_SOCKET: &str = "/run/seedbox/control.sock";

/// One sequential connection to the guest control RPC.
pub struct Control {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    closed: bool,
}

impl Control {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, Error> {
        let stream = UnixStream::connect(path.as_ref())
            .map_err(|e| Error(format!("connect {}: {e}", path.as_ref().display())))?;
        let reader = stream
            .try_clone()
            .map_err(|e| Error(format!("control socket: {e}")))?;
        Ok(Self {
            writer: stream,
            reader: BufReader::new(reader),
            next_id: 1,
            closed: false,
        })
    }

    fn fail<T>(&mut self, message: String) -> Result<T, Error> {
        self.closed = true;
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        Err(Error(message))
    }

    /// Call `op` with an object of `arguments` and return its result object,
    /// or the world's refusal as `op: code: message`, which leaves the
    /// connection usable. The framing fields (`version`, `id`, `op`) are this
    /// client's; arguments naming them are refused before anything is sent.
    pub fn call(&mut self, op: &str, arguments: Value) -> Result<Map<String, Value>, Error> {
        if self.closed {
            return Err(Error("control connection is closed".into()));
        }
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => Map::new(),
            _ => return Err(Error(format!("{op} arguments must be an object"))),
        };
        if let Some(field) = ["version", "id", "op"]
            .iter()
            .find(|field| arguments.contains_key(**field))
        {
            return Err(Error(format!("{op} arguments may not set {field:?}")));
        }
        let id = self.next_id;
        let mut request = json!({"version": 1, "id": id, "op": op});
        request.as_object_mut().unwrap().extend(arguments);
        let mut frame = serde_json::to_vec(&request).expect("requests serialize");
        frame.push(b'\n');
        if frame.len() > MAX_FRAME {
            return Err(Error(format!("{op} request exceeds the control frame")));
        }
        self.next_id += 1;
        if let Err(error) = self.writer.write_all(&frame) {
            return self.fail(format!("control transport: {error}"));
        }
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_FRAME as u64 + 1)
            .read_until(b'\n', &mut line);
        if let Err(error) = read {
            return self.fail(format!("control transport: {error}"));
        }
        if line.len() > MAX_FRAME || line.last() != Some(&b'\n') {
            return self.fail("missing or oversized control response".into());
        }
        let response: Value = match serde_json::from_slice(&line) {
            Ok(response) => response,
            Err(error) => return self.fail(format!("invalid control response: {error}")),
        };
        if response["version"] != 1 || response["id"] != id || !response["ok"].is_boolean() {
            return self.fail("control response correlation mismatch".into());
        }
        if response["ok"] == false {
            let (code, message) = (&response["error"]["code"], &response["error"]["message"]);
            let (Some(code), Some(message)) = (code.as_str(), message.as_str()) else {
                return self.fail("invalid control error".into());
            };
            return Err(Error(format!("{op}: {code}: {message}")));
        }
        match response.get("result") {
            Some(Value::Object(result)) => Ok(result.clone()),
            _ => self.fail("invalid control result".into()),
        }
    }

    fn handle(&mut self, op: &str, arguments: Value) -> Result<String, Error> {
        let result = self.call(op, arguments)?;
        match result.get("handle").and_then(Value::as_str) {
            Some(handle) if !handle.is_empty() => Ok(handle.to_owned()),
            _ => self.fail(format!("{op} response lacks an opaque handle")),
        }
    }
}

fn target(machine: &str) -> Value {
    json!({ "machine": machine })
}

fn targets(machines: &[String]) -> Value {
    Value::Array(machines.iter().map(|m| target(m)).collect())
}

/// A [`World`] over the guest control RPC. Faults that install a handle are
/// named by the caller; the world answers with its own opaque handle, which
/// this maps to, so `Action::Heal` of the caller's name heals that fault.
pub struct GuestWorld {
    control: Control,
    handles: BTreeMap<String, String>,
}

impl GuestWorld {
    /// The oracle's controller socket.
    pub fn connect() -> Result<Self, Error> {
        Self::connect_to(DEFAULT_SOCKET)
    }

    pub fn connect_to(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            control: Control::connect(path)?,
            handles: BTreeMap::new(),
        })
    }

    /// The underlying RPC, for calls outside [`World`] (`inspect`,
    /// `connections`, triggers).
    pub fn control(&mut self) -> &mut Control {
        &mut self.control
    }

    /// Handles still in force: the caller's names and the world's handles.
    pub fn handles(&self) -> &BTreeMap<String, String> {
        &self.handles
    }

    /// Apply an action that installs `handle`, refusing a name in use.
    fn install(&mut self, handle: &str, op: &str, arguments: Value) -> Result<(), Error> {
        if self.handles.contains_key(handle) {
            return Err(Error(format!("handle {handle:?} is already in force")));
        }
        let world = self.control.handle(op, arguments)?;
        self.handles.insert(handle.to_owned(), world);
        Ok(())
    }
}

impl World for GuestWorld {
    fn now_ns(&mut self) -> Result<u64, Error> {
        let result = self.control.call("now", json!({}))?;
        match result.get("now_ns").and_then(Value::as_u64) {
            Some(now) => Ok(now),
            None => self.control.fail("invalid simulated time result".into()),
        }
    }

    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        let duration_ns = u64::try_from(duration.as_nanos())
            .map_err(|_| Error("sleep duration exceeds u64 nanoseconds".into()))?;
        self.control
            .call("sleep", json!({ "duration_ns": duration_ns }))
            .map(drop)
    }

    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        match action {
            Action::Stop { machine }
            | Action::Restart { machine }
            | Action::PauseProcess { machine }
            | Action::ResumeProcess { machine }
            | Action::Pause { machine }
            | Action::Resume { machine }
            | Action::Crash { machine }
            | Action::Reboot { machine } => self
                .control
                .call(action.name(), json!({ "target": target(machine) }))
                .map(drop),
            Action::Partition {
                handle,
                left,
                right,
            } => self.install(
                handle,
                "partition",
                json!({"left": targets(left), "right": targets(right)}),
            ),
            Action::Shape {
                handle,
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
                network_faults,
            } => {
                let mut policy = json!({
                    "from": targets(from), "to": targets(to), "latency_ns": latency_ns,
                    "jitter_ns": jitter_ns, "loss_ppm": loss_ppm,
                });
                if let Some(bandwidth) = bandwidth_bps {
                    policy["bandwidth_bps"] = json!(bandwidth);
                }
                if let Some(bound) = queue_bound_ns {
                    policy["queue_bound_ns"] = json!(bound);
                }
                if !network_faults.is_off() {
                    policy["network_faults"] = json!(network_faults);
                }
                self.install(handle, "shape", policy)
            }
            Action::DropConnection { handle, connection } => self.install(
                handle,
                "drop_connection",
                json!({ "connection": connection }),
            ),
            Action::ClockSkew {
                handle,
                machine,
                offset_ns,
                max_offset_ns,
            } => {
                let mut arguments = json!({ "target": target(machine) });
                match (offset_ns, max_offset_ns) {
                    (Some(offset), None) => arguments["offset_ns"] = json!(offset),
                    (None, Some(bound)) => arguments["max_offset_ns"] = json!(bound),
                    _ => return Err(Error("clock_skew needs exactly one offset".into())),
                }
                self.install(handle, "clock_skew", arguments)
            }
            Action::ClockDrift {
                handle,
                machine,
                rate_ppb,
                max_rate_ppb,
            } => {
                let mut arguments = json!({ "target": target(machine) });
                match (rate_ppb, max_rate_ppb) {
                    (Some(rate), None) => arguments["rate_ppb"] = json!(rate),
                    (None, Some(bound)) => arguments["max_rate_ppb"] = json!(bound),
                    _ => return Err(Error("clock_drift needs exactly one rate".into())),
                }
                self.install(handle, "clock_drift", arguments)
            }
            Action::Throttle {
                handle,
                machine,
                factor_permille,
            } => self.install(
                handle,
                "throttle",
                json!({"target": target(machine), "factor_permille": factor_permille}),
            ),
            Action::DiskError {
                handle,
                machine,
                op,
                ppm,
                effect,
                scope,
            } => {
                let mut arguments = json!({"target": target(machine), "disk_op": op, "ppm": ppm});
                if !effect.is_eio() {
                    arguments["effect"] = json!(effect);
                }
                if !scope.is_all() {
                    arguments["scope"] = json!(scope);
                }
                self.install(handle, "disk_error", arguments)
            }
            Action::DiskLatency {
                handle,
                machine,
                op,
                latency_ns,
                jitter_ns,
            } => {
                let mut arguments =
                    json!({"target": target(machine), "disk_op": op, "latency_ns": latency_ns});
                if *jitter_ns != 0 {
                    arguments["jitter_ns"] = json!(jitter_ns);
                }
                self.install(handle, "disk_latency", arguments)
            }
            Action::Preempt {
                handle,
                machine,
                ppm,
            } => self.install(
                handle,
                "preempt",
                json!({"target": target(machine), "ppm": ppm}),
            ),
            Action::Heal { handle } => {
                let Some(world) = self.handles.remove(handle) else {
                    return Err(Error(format!(
                        "no fault with handle {handle:?} is in force"
                    )));
                };
                // A refused heal names a handle the world no longer has in
                // force; forget it either way rather than keep a stale name.
                self.control
                    .call("heal", json!({ "handle": world }))
                    .map(drop)
            }
        }
    }
}

#[cfg(test)]
mod tests;
