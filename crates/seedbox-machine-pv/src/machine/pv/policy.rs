//! Dedicated named policy streams; never consume the caller's guest entropy.
use super::codec::record::Record;
use crate::definition::BackendPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    pub seed: u64,
    pub timer_jitter_ns: u64,
    pub stop_tolerance_ns: u64,
    pub torn_writes: bool,
    pub full_state: bool,
    pub seeded_pv_speed: bool,
    pub boot_end_evidence: bool,
    pub controller: Option<String>,
}
impl From<&BackendPolicy> for Settings {
    fn from(p: &BackendPolicy) -> Self {
        let BackendPolicy {
            seed,
            timer_jitter_ns,
            stop_tolerance_ns,
            torn_writes,
            full_state,
            seeded_pv_speed,
            boot_end_evidence,
            controller,
        } = p;
        Self {
            seed: *seed,
            timer_jitter_ns: *timer_jitter_ns,
            stop_tolerance_ns: *stop_tolerance_ns,
            torn_writes: *torn_writes,
            full_state: *full_state,
            seeded_pv_speed: *seeded_pv_speed,
            boot_end_evidence: *boot_end_evidence,
            controller: controller.clone(),
        }
    }
}
impl Settings {
    fn validated_without_controller_copy(&self) -> Result<(), String> {
        let policy = BackendPolicy {
            seed: self.seed,
            timer_jitter_ns: self.timer_jitter_ns,
            stop_tolerance_ns: self.stop_tolerance_ns,
            torn_writes: self.torn_writes,
            full_state: self.full_state,
            seeded_pv_speed: self.seeded_pv_speed,
            boot_end_evidence: self.boot_end_evidence,
            controller: None,
        };
        policy.validate().map_err(str::to_owned)?;
        if self.controller.as_ref().is_some_and(String::is_empty) {
            return Err("PV controller name is empty".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Named {
    pub name: String,
    pub settings: Settings,
    pub speed_permille: u64,
}
impl Named {
    pub fn new(name: &str, p: &BackendPolicy) -> Result<Self, String> {
        p.validate().map_err(str::to_owned)?;
        if name.contains('\0') {
            return Err("PV policy name contains NUL".into());
        }
        let mut rng = Counter::named(p.seed, b"pv-speed/", name, None);
        let speed_permille = if p.seeded_pv_speed {
            900 + rng.below(201)?
        } else {
            1000
        };
        Ok(Self {
            name: name.into(),
            settings: Settings::from(p),
            speed_permille,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        self.settings.validated_without_controller_copy()?;
        if self.name.contains('\0') {
            return Err("PV policy name contains NUL".into());
        }
        let mut stream = Counter::named(self.settings.seed, b"pv-speed/", &self.name, None);
        let expected = if self.settings.seeded_pv_speed {
            900 + stream.below(201)?
        } else {
            1000
        };
        if self.speed_permille != expected {
            return Err("PV saved speed differs from named policy".into());
        }
        Ok(())
    }
    pub fn setup(&self, boot: u64) -> BootPolicy {
        BootPolicy {
            name: self.name.clone(),
            seed: self.settings.seed,
            boot,
            timer_jitter_ns: self.settings.timer_jitter_ns,
            stop_tolerance_ns: self.settings.stop_tolerance_ns,
            speed_permille: self.speed_permille,
        }
    }
}
pub(super) fn authority(
    retained: Option<&Named>,
    name: &str,
    expected: Option<&BackendPolicy>,
) -> Result<(), String> {
    match(retained,expected){(None,None)=>Ok(()),(Some(p),Some(e))if p.name==name&&p.settings==Settings::from(e)=>Ok(()),_=>Err("PV snapshot named backend policy differs from factory authority; absence and explicit default are distinct".into())}
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BootPolicy {
    pub name: String,
    pub seed: u64,
    pub boot: u64,
    pub timer_jitter_ns: u64,
    pub stop_tolerance_ns: u64,
    pub speed_permille: u64,
}
impl BootPolicy {
    pub fn validate(&self) -> Result<(), String> {
        BackendPolicy {
            timer_jitter_ns: self.timer_jitter_ns,
            stop_tolerance_ns: self.stop_tolerance_ns,
            ..BackendPolicy::default()
        }
        .validate()
        .map_err(str::to_owned)?;
        if self.name.contains('\0') || !(900..=1100).contains(&self.speed_permille) {
            return Err("PV boot policy name/speed".into());
        }
        Ok(())
    }
    fn timer_stream(&self) -> Counter {
        Counter::named(
            self.seed,
            b"timer-jitter/",
            &self.name,
            (self.boot != 0).then_some(self.boot),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Counter {
    key: [u8; 32],
    counter: u64,
}
impl Counter {
    fn named(seed: u64, prefix: &[u8], name: &str, boot: Option<u64>) -> Self {
        let mut h = Sha256::new();
        h.update(b"seedbox-v1");
        h.update(seed.to_le_bytes());
        h.update(prefix);
        h.update(name.as_bytes());
        if let Some(mut n) = boot {
            let mut digits = [0u8; 20];
            let mut at = 20;
            loop {
                at -= 1;
                digits[at] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            h.update(b"/");
            h.update(&digits[at..]);
        }
        Self {
            key: h.finalize().into(),
            counter: 0,
        }
    }
    pub fn next(&mut self) -> Result<u64, String> {
        let next = self
            .counter
            .checked_add(1)
            .ok_or("PV policy RNG exhausted")?;
        let mut h = Sha256::new();
        h.update(self.key);
        h.update(self.counter.to_le_bytes());
        self.counter = next;
        Ok(u64::from_le_bytes(
            h.finalize()[..8].try_into().expect("eight digest bytes"),
        ))
    }
    fn below(&mut self, n: u64) -> Result<u64, String> {
        if n == 0 {
            return Ok(0);
        }
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let v = self.next()?;
            if v < zone {
                return Ok(v % n);
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Runtime {
    pub setup: BootPolicy,
    timer: Counter,
}
impl Runtime {
    pub fn new(setup: &BootPolicy) -> Result<Self, String> {
        setup.validate()?;
        Ok(Self {
            setup: setup.clone(),
            timer: setup.timer_stream(),
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        self.setup.validate()?;
        if self.timer.key != self.setup.timer_stream().key {
            return Err("PV saved timer stream differs from boot authority".into());
        }
        Ok(())
    }
    pub fn arm(&mut self, value: u64) -> Result<Option<u64>, String> {
        if value == 0 {
            return Ok(None);
        }
        let jitter = if self.setup.timer_jitter_ns == 0 {
            0
        } else {
            self.timer.below(self.setup.timer_jitter_ns + 1)?
        };
        let at = value
            .checked_add(jitter)
            .filter(|v| *v != u64::MAX)
            .ok_or("PV jittered deadline overflow")?;
        Ok(Some(at))
    }
    pub fn charge(&self, base: u64) -> Result<u64, String> {
        let n = (base as u128) * (self.setup.speed_permille as u128);
        u64::try_from(n.div_ceil(1000)).map_err(|_| "PV seeded syscall charge overflow".into())
    }
}
macro_rules! record {($ty:ty {$($field:ident:$kind:ty),*$(,)?})=>{impl Record for $ty{const MIN:u64=0$(+<$kind as Record>::MIN)*;fn encode<W:std::io::Write>(&self,w:&mut seedbox_machine::snapshot::Writer<'_,W>)->seedbox_machine::snapshot::Result<()>{let Self{$($field),*}=self;$($field.encode(w)?;)*Ok(())}fn decode<R:std::io::Read>(r:&mut seedbox_machine::snapshot::Reader<'_,R>)->seedbox_machine::snapshot::Result<Self>{Ok(Self{$($field:<$kind as Record>::decode(r)?),*})}}}}
record!(Settings{seed:u64,timer_jitter_ns:u64,stop_tolerance_ns:u64,torn_writes:bool,full_state:bool,seeded_pv_speed:bool,boot_end_evidence:bool,controller:Option<String>});
record!(Named {
    name: String,
    settings: Settings,
    speed_permille: u64
});
record!(BootPolicy {
    name: String,
    seed: u64,
    boot: u64,
    timer_jitter_ns: u64,
    stop_tolerance_ns: u64,
    speed_permille: u64
});
record!(Counter {
    key: [u8; 32],
    counter: u64
});
record!(Runtime {
    setup: BootPolicy,
    timer: Counter
});
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod tests;
