//! The [`World`] an oracle holds inside a Seedbox guest: the v1 guest
//! control RPC on `/run/seedbox/control.sock`.
//!
//! The RPC is one JSON object per line, at most [`MAX_FRAME`] bytes with its
//! newline, numbered from 1 per connection, as `python/seedbox/control.py`
//! speaks it. Each call blocks until the world confirms its effect; there
//! are no retries, and a transport or protocol failure closes the
//! connection. Only the calls [`World`] needs are here, plus breakpoints
//! ([`GuestWorld::on_break`]).
use crate::{control::Action, Error, World};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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

/// A breakpoint hit. Its machine stays suspended until it is resumed or
/// crashed. A held block request's hit is named `block_request`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub id: u32,
    /// The machine's name, as actions target it.
    pub machine: String,
    pub boot: u64,
    pub name: String,
    /// The simulated time of the breakpoint write or held request.
    pub at_ns: u64,
}

impl Control {
    /// Replace the breakpoint names that suspend a machine, for later hits.
    pub fn declare_breaks<'a>(
        &mut self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), Error> {
        let names: std::collections::BTreeSet<&str> = names.into_iter().collect();
        self.call("declare_breaks", json!({ "names": names }))
            .map(drop)
    }

    /// The oldest hit not yet returned, or `None` once `timeout` of
    /// simulated time passes without one.
    pub fn wait_break(&mut self, timeout: Duration) -> Result<Option<Hit>, Error> {
        let timeout_ns = u64::try_from(timeout.as_nanos())
            .map_err(|_| Error("wait_break timeout exceeds u64 nanoseconds".into()))?;
        let result = self.call("wait_break", json!({ "timeout_ns": timeout_ns }))?;
        if result.is_empty() {
            return Ok(None);
        }
        let number = |key: &str| result.get(key).and_then(Value::as_u64);
        let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_owned);
        match (
            number("break_id").and_then(|id| u32::try_from(id).ok()),
            text("machine"),
            number("boot"),
            text("name"),
            number("at_ns"),
        ) {
            (Some(id), Some(machine), Some(boot), Some(name), Some(at_ns)) => Ok(Some(Hit {
                id,
                machine,
                boot,
                name,
                at_ns,
            })),
            _ => self.fail("invalid breakpoint hit".into()),
        }
    }

    /// Suspend `machine` at its `nth` disk request of kind `op` from now
    /// on, counted across boots, with that request held unwritten. It is
    /// one-shot; its hit is named `block_request`, so
    /// [`GuestWorld::on_break`] with that name serves it. Resuming lets the
    /// request continue.
    pub fn break_on_block_request(
        &mut self,
        machine: &str,
        op: crate::control::DiskOp,
        nth: u64,
    ) -> Result<(), Error> {
        self.call(
            "break_on_block_request",
            json!({ "target": { "machine": machine }, "request": op, "nth": nth }),
        )
        .map(drop)
    }

    /// Let a suspended machine run again; its breakpoint write returns.
    pub fn resume_break(&mut self, hit: &Hit) -> Result<(), Error> {
        self.call("resume_break", json!({ "break_id": hit.id }))
            .map(drop)
    }
}

/// Handles a breakpoint hit while its machine is suspended.
pub type BreakHandler = Box<dyn FnMut(&mut GuestWorld, &Hit) -> Result<(), Error> + Send>;

/// The breakpoint handlers and the thread that serves them.
#[derive(Default)]
struct Breaks {
    handlers: Arc<Mutex<BTreeMap<String, BreakHandler>>>,
    serving: bool,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<Error>>>,
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
    path: PathBuf,
    breaks: Breaks,
}

impl GuestWorld {
    /// The oracle's controller socket.
    pub fn connect() -> Result<Self, Error> {
        Self::connect_to(DEFAULT_SOCKET)
    }

    pub fn connect_to(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            control: Control::connect(path.as_ref())?,
            handles: BTreeMap::new(),
            path: path.as_ref().to_owned(),
            breaks: Breaks::default(),
        })
    }

    /// Call `handler` whenever a machine hits breakpoint `name`.
    ///
    /// The first registration opens a second connection and serves hits on a
    /// background thread, so this world stays free for other calls. The
    /// handler runs while the machine is suspended and gets that thread's
    /// world; the machine resumes when the handler returns, unless the handler
    /// crashed it. A handler error stops serving: its machine stays suspended
    /// and [`Self::break_error`] reports the error.
    pub fn on_break(
        &mut self,
        name: &str,
        handler: impl FnMut(&mut GuestWorld, &Hit) -> Result<(), Error> + Send + 'static,
    ) -> Result<(), Error> {
        let mut handlers = self.breaks.handlers.lock().unwrap();
        let names = handlers.keys().map(String::as_str).chain([name]);
        self.control.declare_breaks(names)?;
        handlers.insert(name.to_owned(), Box::new(handler));
        drop(handlers);
        if !self.breaks.serving {
            let mut world = GuestWorld::connect_to(&self.path)?;
            let handlers = self.breaks.handlers.clone();
            let stop = self.breaks.stop.clone();
            let error = self.breaks.error.clone();
            std::thread::spawn(move || {
                if let Err(failure) = world.serve_breaks(&handlers, &stop) {
                    *error.lock().unwrap() = Some(failure);
                }
            });
            self.breaks.serving = true;
        }
        Ok(())
    }

    /// The error that stopped serving breakpoints, if any.
    pub fn break_error(&self) -> Option<Error> {
        self.breaks.error.lock().unwrap().clone()
    }

    fn serve_breaks(
        &mut self,
        handlers: &Mutex<BTreeMap<String, BreakHandler>>,
        stop: &AtomicBool,
    ) -> Result<(), Error> {
        while !stop.load(Ordering::Relaxed) {
            let Some(hit) = self.control.wait_break(Duration::from_secs(1))? else {
                continue;
            };
            if let Some(handler) = handlers.lock().unwrap().get_mut(&hit.name) {
                handler(self, &hit)?;
            }
            match self.control.resume_break(&hit) {
                // The handler crashed the machine, which ended the hit.
                Err(Error(message)) if message.starts_with("resume_break: invalid_state:") => {}
                result => result?,
            }
        }
        Ok(())
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

impl Drop for GuestWorld {
    fn drop(&mut self) {
        self.breaks.stop.store(true, Ordering::Relaxed);
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
