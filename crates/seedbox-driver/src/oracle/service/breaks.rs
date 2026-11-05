//! Breakpoints: the declared names, live hits and parked `wait_break` calls.
//! A hit suspends its machine from the commit that reported it until the
//! oracle resumes it, the machine crashes or the run ends. Hits are numbered
//! in commit order and returned to waiters oldest first.
use super::*;
use std::collections::BTreeSet;

/// Why a hit ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BreakEnd {
    Resumed,
    Crashed,
    RunEnded,
}

/// The name of every block request hit. It is reserved: a program cannot
/// declare it, so a hit with this name always comes from a held request.
pub(crate) const BLOCK_REQUEST: &str = "block_request";

/// What suspended a machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HitSource {
    /// The guest wrote this declared name to `/dev/seedbox/break`.
    Program(String),
    /// Its device holds the `nth` disk request of kind `op`, unwritten.
    BlockRequest { op: crate::pv::DiskOp, nth: u64 },
}
impl HitSource {
    /// The name the hit carries; a block request hit is named `block_request`.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Program(name) => name,
            Self::BlockRequest { .. } => BLOCK_REQUEST,
        }
    }
}

/// A live hit: its machine is suspended at a breakpoint write or a held
/// block request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    /// The machine's canonical key.
    pub(crate) machine: String,
    /// The machine's display name, as replies name it.
    pub(crate) display: String,
    pub(crate) boot: u64,
    pub(crate) source: HitSource,
    pub(crate) at: u64,
    /// A `wait_break` call has returned this hit.
    pub(crate) delivered: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Breaks {
    /// Set once by the Driver's owner when its engine can suspend machines.
    pub(crate) supported: bool,
    declared: BTreeSet<String>,
    last: u32,
    hits: BTreeMap<u32, Hit>,
    /// Parked `wait_break` calls by wake token. A hit that answered one
    /// leaves `None` until its timeout wake retires the token.
    waiters: BTreeMap<u32, Option<Call>>,
}

impl Breaks {
    /// The declared names, ascending.
    pub(crate) fn declared(&self) -> Vec<String> {
        self.declared.iter().cloned().collect()
    }
    /// Whether the machine with this canonical key is suspended.
    pub(crate) fn suspended(&self, machine: &str) -> bool {
        self.hits.values().any(|hit| hit.machine == machine)
    }
    /// Whether `break_id` has been allocated.
    pub(crate) fn allocated(&self, break_id: u32) -> bool {
        break_id > 0 && break_id <= self.last
    }
    pub(crate) fn hits(&self) -> impl Iterator<Item = (u32, &Hit)> {
        self.hits.iter().map(|(&id, hit)| (id, hit))
    }
    pub(super) fn json(id: u32, hit: &Hit) -> Value {
        let mut value = json!({
            "break_id": id,
            "source": "program",
            "machine": hit.display,
            "boot": hit.boot,
            "name": hit.source.name(),
            "at_ns": hit.at,
        });
        if let HitSource::BlockRequest { op, nth } = hit.source {
            value["source"] = json!(BLOCK_REQUEST);
            value["request"] = json!(op);
            value["nth"] = json!(nth);
        }
        value
    }
}

impl Service {
    fn breaks_supported(&self) -> Result<(), OperationError> {
        if self.breaks.supported {
            Ok(())
        } else {
            Err(error(
                ErrorCode::Unsupported,
                "breakpoints are not supported by this engine",
            ))
        }
    }
    /// Replace the declared set; every machine gets it at this barrier.
    pub(super) fn declare_breaks(
        &mut self,
        call: Call,
        now: u64,
        names: Vec<String>,
    ) -> Result<Vec<Plan>, Failure> {
        if let Err(e) = self.breaks_supported() {
            return Ok(vec![self.reply(call, now, None, Err(e))?]);
        }
        self.breaks.declared = names.into_iter().collect();
        Ok(vec![
            Plan::DeclareBreaks {
                names: self.breaks.declared(),
                at: now,
            },
            self.reply(call, now, Some(now), Ok(json!({})))?,
        ])
    }
    /// Return the oldest undelivered hit, or park until one arrives or the
    /// timeout passes.
    pub(super) fn wait_break(
        &mut self,
        call: Call,
        now: u64,
        timeout_ns: u64,
    ) -> Result<Vec<Plan>, Failure> {
        if let Err(e) = self.breaks_supported() {
            return Ok(vec![self.reply(call, now, None, Err(e))?]);
        }
        if let Some((&id, hit)) = self.breaks.hits.iter_mut().find(|(_, h)| !h.delivered) {
            hit.delivered = true;
            let result = Breaks::json(id, hit);
            return Ok(vec![self.reply(call, now, Some(now), Ok(result))?]);
        }
        self.last_wake = self
            .last_wake
            .checked_add(1)
            .ok_or("sleep wake IDs exhausted")?;
        let token = self.last_wake;
        self.breaks.waiters.insert(token, Some(call));
        Ok(vec![Plan::Wake {
            token,
            at: now + timeout_ns,
        }])
    }
    /// Plan a one-shot hold of `target`'s `nth` disk request of kind `op`,
    /// counted from this barrier across boots. The hit arrives like any
    /// other, with source `block_request`.
    pub(super) fn break_on_block_request(
        &mut self,
        adapter: &mut impl Adapter,
        target: &Target,
        op: crate::pv::DiskOp,
        nth: u64,
        now: u64,
    ) -> Result<Plan, OperationError> {
        self.breaks_supported()?;
        adapter.check_control_target(target)?;
        let machine = adapter.resolve(target)?.name;
        Ok(Plan::BreakOnBlockRequest {
            machine,
            op,
            nth,
            at: now,
        })
    }
    /// End a live hit and let its machine run again.
    pub(super) fn resume_break(
        &mut self,
        break_id: u32,
        now: u64,
    ) -> Result<(Value, Vec<Plan>), OperationError> {
        self.breaks_supported()?;
        let hit = self.breaks.hits.remove(&break_id).ok_or_else(|| {
            error(
                ErrorCode::InvalidState,
                if break_id == 0 || break_id > self.breaks.last {
                    "unknown break_id"
                } else {
                    "breakpoint hit has already ended"
                },
            )
        })?;
        self.notices.push(Notice::BreakEnded {
            break_id,
            at: now,
            reason: BreakEnd::Resumed,
        });
        Ok((
            json!({}),
            vec![Plan::ResumeBreak {
                machine: hit.machine,
                boot: hit.boot,
                at: now,
            }],
        ))
    }
    /// Record a committed hit. The oldest parked waiter, if any, returns it.
    pub(crate) fn break_hit(
        &mut self,
        machine: String,
        display: String,
        boot: u64,
        source: HitSource,
        at: u64,
        now: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        if !self.breaks.supported {
            return Err("breakpoint hit on an engine without breakpoints".into());
        }
        if self.breaks.suspended(&machine) {
            return Err("breakpoint hit on a suspended machine".into());
        }
        self.breaks.last = self
            .breaks
            .last
            .checked_add(1)
            .ok_or("break IDs exhausted")?;
        let id = self.breaks.last;
        let mut hit = Hit {
            machine,
            display,
            boot,
            source,
            at,
            delivered: false,
        };
        self.notices.push(Notice::BreakHit {
            break_id: id,
            machine: hit.machine.clone(),
            boot,
            name: hit.source.name().to_owned(),
            at,
        });
        let waiter = self
            .breaks
            .waiters
            .values_mut()
            .find(|waiter| waiter.is_some())
            .and_then(Option::take);
        let plans = match waiter {
            Some(call) => {
                hit.delivered = true;
                vec![self.reply(call, now, Some(now), Ok(Breaks::json(id, &hit)))?]
            }
            None => vec![],
        };
        self.breaks.hits.insert(id, hit);
        Ok(plans)
    }
    /// End the live hit of a machine that crashed or rebooted.
    pub(crate) fn break_crashed(&mut self, machine: &str, at: u64) {
        let ended: Vec<u32> = self
            .breaks
            .hits
            .iter()
            .filter(|(_, hit)| hit.machine == machine)
            .map(|(&id, _)| id)
            .collect();
        for break_id in ended {
            self.breaks.hits.remove(&break_id);
            self.notices.push(Notice::BreakEnded {
                break_id,
                at,
                reason: BreakEnd::Crashed,
            });
        }
    }
    /// The run ended: suspended machines stay suspended and every hit ends.
    pub(crate) fn cut_off_breaks(&mut self, at: u64) {
        for break_id in std::mem::take(&mut self.breaks.hits).into_keys() {
            self.notices.push(Notice::BreakEnded {
                break_id,
                at,
                reason: BreakEnd::RunEnded,
            });
        }
    }
    /// A `wait_break` timeout, or `None` when this token is not a waiter.
    /// A waiter a hit already answered retires without a reply.
    pub(super) fn break_timeout(
        &mut self,
        token: u32,
        at: u64,
    ) -> Option<Result<Vec<Plan>, Failure>> {
        let waiter = self.breaks.waiters.remove(&token)?;
        Some(match waiter {
            Some(call) => self
                .reply(call, at, Some(at), Ok(json!({})))
                .map(|p| vec![p]),
            None => Ok(vec![]),
        })
    }
}

wire_enum!(BreakEnd; units {0=>Resumed,1=>Crashed,2=>RunEnded} tuples {} records {});
wire_enum!(HitSource; units {} tuples {0=>Program(v0)} records {1=>BlockRequest{op,nth}});
wire_struct!(Hit {
    machine,
    display,
    boot,
    source,
    at,
    delivered
});
wire_struct!(Breaks {
    supported,
    declared,
    last,
    hits,
    waiters
});

impl Breaks {
    pub(super) fn check_persist(
        &self,
        now: u64,
        last_wake: u32,
        exists: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        // No checkpoint is taken while a machine is suspended.
        require(self.hits.is_empty(), "machine suspended at a breakpoint")?;
        require(
            self.supported || (self.declared.is_empty() && self.waiters.is_empty()),
            "breakpoints without engine support",
        )?;
        require(
            self.declared
                .iter()
                .all(|name| crate::pv::break_name_valid(name.as_bytes())),
            "declared breakpoint name",
        )?;
        for (&token, waiter) in &self.waiters {
            require(token > 0 && token <= last_wake, "break waiter token")?;
            if let Some(c) = waiter {
                require(
                    exists(&c.source) && c.requested <= c.admitted && c.admitted <= now,
                    "break waiter",
                )?;
            }
        }
        Ok(())
    }
}
