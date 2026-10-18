//! Blocking, single-connection client for the Seedbox v1 guest control RPC.
//!
//! This mirrors `python/seedbox/control.py`: one JSON object per line, at most
//! [`MAX_FRAME`] bytes including the newline, request IDs from one, and a
//! response that must echo the request's version and ID and carry four ordered
//! effect timestamps. Calls block the calling thread until the simulator's
//! confirmed response arrives; other guests keep running. There are no
//! automatic retries or implicit heals: a transport or protocol failure closes
//! the connection, and the caller decides whether a new session is safe.
//! Request bodies are built from the `seedbox-control` schema types.
use seedbox_control::{Connection, NetworkFaults, Operation, Request, TriggerFault};
use serde_json::{Map, Value};
use std::{
    fmt,
    io::{BufRead, BufReader, Read, Write},
    num::NonZeroU32,
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

pub use seedbox_control::{Target, MAX_CLOCK_OFFSET_NS, MAX_CLOCK_RATE_PPB};

/// Largest request or response line, newline included.
pub const MAX_FRAME: usize = 16_384;
/// Where a guest finds its controller socket.
pub const DEFAULT_SOCKET: &str = "/run/seedbox/control.sock";
const MAX_LINK_DELAY_NS: u64 = 3_600_000_000_000;

/// Why a call did not return a result.
#[derive(Debug)]
pub enum Error {
    /// The arguments were refused before anything was sent.
    Invalid(String),
    /// The peer disconnected or answered with an invalid response; the
    /// connection is now closed.
    Protocol(String),
    /// The simulator refused the operation; its structured error is kept.
    Fault {
        code: String,
        message: String,
        response: Value,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Protocol(message) => f.write_str(message),
            Self::Fault { code, message, .. } => write!(f, "{code}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(message.into()))
}

/// The four simulated times every response carries, in nondecreasing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effect {
    pub requested_at_ns: u64,
    pub admitted_at_ns: u64,
    pub effect_at_ns: u64,
    pub replied_at_ns: u64,
}

/// A healable fault handle; [`World::heal`] releases only this layer.
pub trait Handle {
    fn handle(&self) -> &str;
}

macro_rules! handle {
    ($(#[$doc:meta])* $name:ident { $($field:ident: $type:ty),* }) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name {
            pub handle: String,
            $(pub $field: $type,)*
            pub effect: Effect,
        }
        impl Handle for $name {
            fn handle(&self) -> &str {
                &self.handle
            }
        }
    };
}

handle!(
    /// A bidirectional cut between two target sets.
    Partition {}
);
handle!(
    /// One absolute directed link-policy layer.
    Shape {}
);
handle!(
    /// A CPU throttle on one machine.
    Throttle {}
);
handle!(
    /// Armed loss for one discovered connection incarnation.
    ConnectionDrop {}
);
handle!(
    /// A wall-clock step; `offset_ns` is the offset actually applied.
    ClockSkew { offset_ns: i64 }
);
handle!(
    /// A clock rate error; `rate_ppb` is the rate actually applied.
    ClockDrift { rate_ppb: i64 }
);

/// An armed console trigger. A partition trigger carries the handle its
/// partition will have once it fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub id: u32,
    pub effect: Effect,
    pub partition: Option<Partition>,
}

/// The confirmed release of a handle and the simulator's result document,
/// which holds drop or queue evidence for connection drops and shapes.
#[derive(Clone, Debug, PartialEq)]
pub struct Healed {
    pub effect: Effect,
    pub result: Map<String, Value>,
}

/// One page of open directional connection incarnations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionPage {
    pub connections: Vec<Connection>,
    pub next_after: u64,
}

/// A prepared machine named explicitly; plain strings name services.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine(pub String);

/// Anything that names a fault target.
pub trait IntoTarget {
    fn into_target(self) -> Target;
}
impl IntoTarget for Target {
    fn into_target(self) -> Target {
        self
    }
}
impl IntoTarget for &Target {
    fn into_target(self) -> Target {
        self.clone()
    }
}
impl IntoTarget for &str {
    fn into_target(self) -> Target {
        Target::Service(self.into())
    }
}
impl IntoTarget for String {
    fn into_target(self) -> Target {
        Target::Service(self)
    }
}
impl IntoTarget for &String {
    fn into_target(self) -> Target {
        Target::Service(self.clone())
    }
}
impl IntoTarget for Machine {
    fn into_target(self) -> Target {
        Target::Machine(self.0)
    }
}
impl IntoTarget for &Machine {
    fn into_target(self) -> Target {
        Target::Machine(self.0.clone())
    }
}

fn target(value: impl IntoTarget) -> Result<Target> {
    let target = value.into_target();
    match &target {
        Target::Service(name) | Target::Machine(name) if !name.is_empty() => Ok(target),
        _ => invalid("targets must be nonempty service names or Machine values"),
    }
}

fn targets<T: IntoTarget>(values: impl IntoIterator<Item = T>) -> Result<Vec<Target>> {
    values.into_iter().map(target).collect()
}

fn nanoseconds(duration: Duration) -> Result<u64> {
    u64::try_from(duration.as_nanos())
        .or_else(|_| invalid("duration must fit whole unsigned 64-bit nanoseconds"))
}

/// A CPU speed for [`World::throttle`]: whole Hz, or a frequency string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Speed {
    Hz(u64),
    Frequency(String),
}
impl From<u64> for Speed {
    fn from(hz: u64) -> Self {
        Self::Hz(hz)
    }
}
impl From<&str> for Speed {
    fn from(text: &str) -> Self {
        Self::Frequency(text.into())
    }
}

/// Parse `500MHz`, `1.5GHz` or whole Hz into Hz, from 1 kHz to 1 THz.
pub fn speed_hz(speed: &Speed) -> Result<u64> {
    let hz = match speed {
        Speed::Hz(hz) => *hz,
        Speed::Frequency(text) => parse_frequency(text.trim()).ok_or_else(|| {
            Error::Invalid("throttle speed must be a frequency such as 500MHz or 1.5GHz".into())
        })??,
    };
    if !(1_000..=1_000_000_000_000).contains(&hz) {
        return invalid("throttle speed must be from 1kHz to 1THz");
    }
    Ok(hz)
}

/// `None` when the text is not `digits[.digits]unit`; an error when it is
/// not a whole number of Hz.
fn parse_frequency(text: &str) -> Option<Result<u64>> {
    let digits = text
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    let (number, unit) = text.split_at(digits);
    let scale: u32 = match unit {
        "Hz" => 0,
        "kHz" => 3,
        "MHz" => 6,
        "GHz" => 9,
        "THz" => 12,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || (number.contains('.') && fraction.is_empty())
        || fraction.contains('.')
        || !whole.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > scale as usize {
        return Some(invalid("throttle speed must be a whole number of Hz"));
    }
    let digits = format!("{whole}{fraction:0<width$}", width = scale as usize);
    // Values beyond u64 are out of range anyway.
    Some(Ok(digits.trim_start_matches('0').parse().unwrap_or(
        if digits.bytes().all(|b| b == b'0') {
            0
        } else {
            u64::MAX
        },
    )))
}

/// Optional parts of a [`World::shape`] policy; latency and delays are nanoseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShapeOptions {
    pub jitter: u64,
    pub loss_ppm: u64,
    pub bandwidth_bps: Option<u64>,
    pub queue_bound: Option<u64>,
    pub faults: NetworkFaults,
}

/// Exactly one of a fixed nonzero value or a positive bound the simulator
/// draws from on the machine's seeded clock stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amount {
    Fixed(i64),
    Bound(u64),
}

/// One sequential RPC session inside a simulated guest. Do not share one
/// session across threads; open a new one with [`World::connect`] instead.
/// Fault handles survive graceful disconnects.
pub struct World {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    closed: bool,
    /// The timings of the most recent response, including refusals.
    pub last_effect: Option<Effect>,
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("next_id", &self.next_id)
            .field("closed", &self.closed)
            .finish()
    }
}

impl World {
    /// Open a new controller session at the default socket.
    pub fn connect_default() -> std::io::Result<Self> {
        Self::connect(DEFAULT_SOCKET)
    }

    /// Open a new controller session; never retry an uncertain prior call.
    pub fn connect(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_stream(UnixStream::connect(path)?)
    }

    /// Use an already connected stream as a new session with IDs from one.
    pub fn from_stream(stream: UnixStream) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            closed: false,
            last_effect: None,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.writer.shutdown(std::net::Shutdown::Both);
        }
    }

    fn protocol<T>(&mut self, message: impl Into<String>) -> Result<T> {
        self.close();
        Err(Error::Protocol(message.into()))
    }

    fn call(&mut self, operation: Operation) -> Result<(Map<String, Value>, Effect)> {
        if self.closed {
            return Err(Error::Protocol("control connection is closed".into()));
        }
        let Some(id) = u32::try_from(self.next_id).ok().and_then(NonZeroU32::new) else {
            return Err(Error::Protocol("request IDs exhausted".into()));
        };
        let request = Request {
            version: 1,
            id,
            operation,
        };
        let mut frame = ascii_json(&serde_json::to_string(&request).map_err(|error| {
            Error::Invalid(format!("control request cannot be encoded: {error}"))
        })?);
        frame.push('\n');
        if frame.len() > MAX_FRAME {
            return invalid("control request exceeds 16384-byte frame limit");
        }
        self.next_id += 1;
        let line = match self.exchange(frame.as_bytes()) {
            Ok(line) => line,
            Err(error) => return self.protocol(format!("control transport failed: {error}")),
        };
        if line.len() > MAX_FRAME || !line.ends_with(b"\n") {
            return self.protocol("missing or oversized control response");
        }
        let response: Value = match std::str::from_utf8(&line)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str(text).map_err(|e| e.to_string()))
        {
            Ok(value) => value,
            Err(error) => return self.protocol(format!("control transport failed: {error}")),
        };
        let Value::Object(mut response) = response else {
            return self.protocol("unsupported control response version");
        };
        if response.get("version").and_then(Value::as_u64) != Some(1) {
            return self.protocol("unsupported control response version");
        }
        let ok = match (
            response.get("id").and_then(Value::as_u64),
            response.get("ok"),
        ) {
            (Some(echo), Some(Value::Bool(ok))) if echo == u64::from(id.get()) => *ok,
            _ => return self.protocol("control response correlation mismatch"),
        };
        let mut times = [0u64; 4];
        for (slot, key) in times.iter_mut().zip([
            "requested_at_ns",
            "admitted_at_ns",
            "effect_at_ns",
            "replied_at_ns",
        ]) {
            match response.get(key).and_then(Value::as_u64) {
                Some(value) => *slot = value,
                None => return self.protocol("invalid control effect timing"),
            }
        }
        if times.windows(2).any(|pair| pair[0] > pair[1]) {
            return self.protocol("invalid control effect timing");
        }
        let effect = Effect {
            requested_at_ns: times[0],
            admitted_at_ns: times[1],
            effect_at_ns: times[2],
            replied_at_ns: times[3],
        };
        self.last_effect = Some(effect);
        if !ok {
            let error = response.get("error");
            let field = |name: &str| {
                error
                    .and_then(|e| e.get(name))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            return match (field("code"), field("message")) {
                (Some(code), Some(message)) => Err(Error::Fault {
                    code,
                    message,
                    response: Value::Object(response),
                }),
                _ => self.protocol("invalid control error"),
            };
        }
        match response.remove("result") {
            Some(Value::Object(result)) => Ok((result, effect)),
            _ => self.protocol("invalid control result"),
        }
    }

    fn exchange(&mut self, frame: &[u8]) -> std::io::Result<Vec<u8>> {
        self.writer.write_all(frame)?;
        self.writer.flush()?;
        let mut line = Vec::new();
        (&mut self.reader)
            .take(MAX_FRAME as u64 + 1)
            .read_until(b'\n', &mut line)?;
        Ok(line)
    }

    fn handle_of(&mut self, result: &Map<String, Value>, what: &str) -> Result<String> {
        match result.get("handle").and_then(Value::as_str) {
            Some(handle) if !handle.is_empty() => Ok(handle.into()),
            _ => self.protocol(format!("{what} response lacks an opaque handle")),
        }
    }

    /// The simulated time at which the request took effect.
    pub fn now_ns(&mut self) -> Result<u64> {
        let (result, effect) = self.call(Operation::Now {})?;
        match result.get("now_ns").and_then(Value::as_u64) {
            Some(now) if now == effect.effect_at_ns => Ok(now),
            _ => self.protocol("invalid simulated time result"),
        }
    }

    /// Topology and simulator-owned state at the admission boundary.
    pub fn inspect(&mut self) -> Result<Map<String, Value>> {
        Ok(self.call(Operation::Inspect {})?.0)
    }

    /// Wait at least this long in simulated time, plus RPC delivery latency.
    pub fn sleep(&mut self, duration: Duration) -> Result<Effect> {
        let duration_ns = nanoseconds(duration)?;
        Ok(self.call(Operation::Sleep { duration_ns })?.1)
    }

    /// Start no generated fault for this long in simulated time; teardowns
    /// of generated faults already active still run. Returns the simulated
    /// time, in nanoseconds, at which the quiet period ends. Unlike
    /// `seedbox_sdk::control::quiet`, this uses the open connection: the
    /// guest relay serves one controller connection at a time, so a second
    /// connection waits until this one closes.
    pub fn quiet(&mut self, duration: Duration) -> Result<u64> {
        let duration_ns = nanoseconds(duration)?;
        let (result, _) = self.call(Operation::Quiet { duration_ns })?;
        match result.get("quiet_until_ns").and_then(Value::as_u64) {
            Some(until) => Ok(until),
            None => self.protocol("invalid quiet result"),
        }
    }

    /// Return only after the application process group has been stopped.
    pub fn stop(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Stop { target })?.1)
    }

    /// Return after a successful exec; readiness remains the application's check.
    pub fn restart(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Restart { target })?.1)
    }

    /// SIGSTOP the application's process group; its kernel keeps running.
    pub fn pause_process(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::PauseProcess { target })?.1)
    }

    /// SIGCONT a process group frozen by [`World::pause_process`].
    pub fn resume_process(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::ResumeProcess { target })?.1)
    }

    /// Freeze the target's whole machine; other machines keep running.
    pub fn pause(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Pause { target })?.1)
    }

    /// Continue a paused machine; missed timer ticks fire once, late.
    pub fn resume(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Resume { target })?.1)
    }

    /// Destroy the machine: RAM, sockets and queued I/O are lost.
    pub fn crash(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Crash { target })?.1)
    }

    /// Boot a crashed machine again from its image.
    pub fn reboot(&mut self, target: impl IntoTarget) -> Result<Effect> {
        let target = self::target(target)?;
        Ok(self.call(Operation::Reboot { target })?.1)
    }

    /// Cut traffic in both directions and return an independent handle.
    pub fn partition<L: IntoTarget, R: IntoTarget>(
        &mut self,
        left: impl IntoIterator<Item = L>,
        right: impl IntoIterator<Item = R>,
    ) -> Result<Partition> {
        let (left, right) = (targets(left)?, targets(right)?);
        let (result, effect) = self.call(Operation::Partition { left, right })?;
        let handle = self.handle_of(&result, "partition")?;
        Ok(Partition { handle, effect })
    }

    /// Apply an absolute directed policy; latency and jitter are nanoseconds.
    pub fn shape<F: IntoTarget, T: IntoTarget>(
        &mut self,
        from: impl IntoIterator<Item = F>,
        to: impl IntoIterator<Item = T>,
        latency: u64,
        options: ShapeOptions,
    ) -> Result<Shape> {
        let (from, to) = (targets(from)?, targets(to)?);
        let ShapeOptions {
            jitter,
            loss_ppm,
            bandwidth_bps,
            queue_bound,
            faults,
        } = options;
        if loss_ppm > 1_000_000 {
            return invalid("loss_ppm must be an integer in [0, 1000000]");
        }
        if jitter >= latency || latency.saturating_add(jitter) > MAX_LINK_DELAY_NS {
            return invalid("jitter must be less than latency; their sum must not exceed one hour");
        }
        if bandwidth_bps == Some(0) {
            return invalid("bandwidth_bps must be a positive u64 integer");
        }
        if queue_bound.is_some() && bandwidth_bps.is_none() {
            return invalid(
                "queue_bound must be a nonnegative u64 integer and requires bandwidth_bps",
            );
        }
        for (name, value) in [
            ("reorder_ppm", faults.reorder_ppm),
            ("duplicate_ppm", faults.duplicate_ppm),
            ("burst_loss_ppm", faults.burst_loss_ppm),
            ("latency_spike_ppm", faults.latency_spike_ppm),
            ("tcp_reset_ppm", faults.tcp_reset_ppm),
        ] {
            if value > 1_000_000 {
                return invalid(format!("{name} must be an integer in [0, 1000000]"));
            }
        }
        for (probability, amount) in [
            (faults.reorder_ppm, faults.reorder_delay_ns),
            (faults.burst_loss_ppm, faults.burst_length),
            (faults.latency_spike_ppm, faults.latency_spike_ns),
        ] {
            if probability != 0 && amount == 0 {
                return invalid("active reorder, burst loss and latency spikes require a positive delay or length");
            }
        }
        let reorder = if faults.reorder_ppm != 0 {
            faults.reorder_delay_ns
        } else {
            0
        };
        let spike = if faults.latency_spike_ppm != 0 {
            faults.latency_spike_ns
        } else {
            0
        };
        if latency
            .checked_add(jitter)
            .and_then(|n| n.checked_add(reorder))
            .and_then(|n| n.checked_add(spike))
            .is_none_or(|n| n > MAX_LINK_DELAY_NS)
        {
            return invalid("latency, jitter and fault delays must not exceed one hour");
        }
        let (result, effect) = self.call(Operation::Shape {
            from,
            to,
            latency_ns: latency,
            jitter_ns: jitter,
            loss_ppm,
            bandwidth_bps,
            queue_bound_ns: queue_bound,
            network_faults: faults,
        })?;
        let handle = self.handle_of(&result, "shape")?;
        Ok(Shape { handle, effect })
    }

    /// Up to 32 open directional incarnations after `after`, and the next cursor.
    /// Discovery is a live admission-time page, not a frozen inventory.
    pub fn connections(
        &mut self,
        from: impl IntoTarget,
        to: impl IntoTarget,
        after: u64,
    ) -> Result<ConnectionPage> {
        let (from, to) = (target(from)?, target(to)?);
        let (result, _) = self.call(Operation::Connections { from, to, after })?;
        let connections = result
            .get("connections")
            .cloned()
            .map(serde_json::from_value::<Vec<Connection>>);
        match (
            connections,
            result.get("next_after").and_then(Value::as_u64),
        ) {
            (Some(Ok(connections)), Some(next_after)) => Ok(ConnectionPage {
                connections,
                next_after,
            }),
            _ => self.protocol("invalid connections result"),
        }
    }

    /// Arm loss for one discovered direction and incarnation. Arming does not
    /// prove a packet was dropped; the heal result reports drop evidence.
    pub fn drop_connection(&mut self, connection: &Connection) -> Result<ConnectionDrop> {
        let (result, effect) = self.call(Operation::DropConnection {
            connection: connection.clone(),
        })?;
        let handle = self.handle_of(&result, "drop_connection")?;
        Ok(ConnectionDrop { handle, effect })
    }

    /// Step the machine's CLOCK_REALTIME; monotonic time is untouched. A bound
    /// draws the offset uniformly from [-bound, bound]; heal steps it back.
    pub fn clock_skew(&mut self, target: impl IntoTarget, offset: Amount) -> Result<ClockSkew> {
        let target = self::target(target)?;
        let (offset_ns, max_offset_ns) = match offset {
            Amount::Fixed(value) if value != 0 && value.unsigned_abs() <= MAX_CLOCK_OFFSET_NS => {
                (Some(value), None)
            }
            Amount::Bound(value) if value != 0 && value <= MAX_CLOCK_OFFSET_NS => {
                (None, Some(value))
            }
            _ => {
                return invalid(format!("offset must be a nonzero integer and max_offset positive, each at most {MAX_CLOCK_OFFSET_NS} ns"))
            }
        };
        let (result, effect) = self.call(Operation::ClockSkew {
            target,
            offset_ns,
            max_offset_ns,
        })?;
        let handle = self.handle_of(&result, "clock_skew")?;
        match result.get("offset_ns").and_then(Value::as_i64) {
            Some(offset_ns) => Ok(ClockSkew {
                handle,
                offset_ns,
                effect,
            }),
            None => self.protocol("clock_skew response lacks an opaque handle or offset"),
        }
    }

    /// Run the machine's clock fast (or slow if negative) by a rate error in
    /// parts per billion; heal restores the nominal rate.
    pub fn clock_drift(&mut self, target: impl IntoTarget, rate: Amount) -> Result<ClockDrift> {
        let target = self::target(target)?;
        let (rate_ppb, max_rate_ppb) = match rate {
            Amount::Fixed(value) if value != 0 && value.unsigned_abs() <= MAX_CLOCK_RATE_PPB => {
                (Some(value), None)
            }
            Amount::Bound(value) if value != 0 && value <= MAX_CLOCK_RATE_PPB => {
                (None, Some(value))
            }
            _ => {
                return invalid(format!("ppb must be a nonzero integer and max_ppb positive, each at most {MAX_CLOCK_RATE_PPB}"))
            }
        };
        let (result, effect) = self.call(Operation::ClockDrift {
            target,
            rate_ppb,
            max_rate_ppb,
        })?;
        let handle = self.handle_of(&result, "clock_drift")?;
        match result.get("rate_ppb").and_then(Value::as_i64) {
            Some(rate_ppb) => Ok(ClockDrift {
                handle,
                rate_ppb,
                effect,
            }),
            None => self.protocol("clock_drift response lacks an opaque handle or rate"),
        }
    }

    /// Run the target's machine at `speed` until healed.
    pub fn throttle(
        &mut self,
        target: impl IntoTarget,
        speed: impl Into<Speed>,
    ) -> Result<Throttle> {
        let target = self::target(target)?;
        let speed_hz = speed_hz(&speed.into())?;
        let (result, effect) = self.call(Operation::Throttle { target, speed_hz })?;
        let handle = self.handle_of(&result, "throttle")?;
        Ok(Throttle { handle, effect })
    }

    /// Release this handle only; other layers and overlapping cuts remain.
    pub fn heal(&mut self, handle: &impl Handle) -> Result<Healed> {
        let (result, effect) = self.call(Operation::Heal {
            handle: handle.handle().into(),
        })?;
        Ok(Healed { effect, result })
    }

    fn arm(
        &mut self,
        source: impl IntoTarget,
        marker: &[u8],
        match_timeout: Duration,
        fault: TriggerFault,
    ) -> Result<Trigger> {
        let source = target(source)?;
        if !seedbox_control::valid_trigger_marker(marker) {
            return invalid("marker must contain 1..=256 bytes");
        }
        let match_timeout_ns = nanoseconds(match_timeout)?;
        if match_timeout_ns == 0 {
            return invalid("match timeout must be positive");
        }
        let partition = matches!(fault, TriggerFault::Partition { .. });
        let (result, effect) = self.call(Operation::ArmTrigger {
            source,
            marker: marker.to_vec(),
            match_timeout_ns,
            fault,
        })?;
        let id = match result.get("trigger_id").and_then(Value::as_u64) {
            Some(id @ 1..=64) => id as u32,
            _ => return self.protocol("arm response lacks a bounded trigger ID"),
        };
        let partition = if partition {
            match result.get("handle").and_then(Value::as_str) {
                Some(handle) if !handle.is_empty() => Some(Partition {
                    handle: handle.into(),
                    effect,
                }),
                _ => return self.protocol("arm response lacks a partition handle"),
            }
        } else {
            None
        };
        Ok(Trigger {
            id,
            effect,
            partition,
        })
    }

    /// Arm once on future console bytes; confirmation means armed, not stopped.
    pub fn arm_console_stop(
        &mut self,
        source: impl IntoTarget,
        marker: &[u8],
        target: impl IntoTarget,
        match_timeout: Duration,
    ) -> Result<Trigger> {
        let target = self::target(target)?;
        self.arm(source, marker, match_timeout, TriggerFault::Stop { target })
    }

    /// Arm a future partition; its handle can be healed after the effect.
    pub fn arm_console_partition<L: IntoTarget, R: IntoTarget>(
        &mut self,
        source: impl IntoTarget,
        marker: &[u8],
        left: impl IntoIterator<Item = L>,
        right: impl IntoIterator<Item = R>,
        match_timeout: Duration,
    ) -> Result<Trigger> {
        let (left, right) = (targets(left)?, targets(right)?);
        self.arm(
            source,
            marker,
            match_timeout,
            TriggerFault::Partition { left, right },
        )
    }

    /// Cancel at admission; a matched, fired or expired trigger cannot cancel.
    pub fn cancel_trigger(&mut self, trigger: &Trigger) -> Result<Effect> {
        Ok(self
            .call(Operation::CancelTrigger {
                trigger_id: trigger.id,
            })?
            .1)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.close();
    }
}

/// Escape every non-ASCII character as JSON `\u` escapes, as Python's
/// `ensure_ascii` does; serde_json only emits them inside strings.
fn ascii_json(text: &str) -> String {
    if text.is_ascii() {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
