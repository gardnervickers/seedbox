//! Factory policy identity and dedicated atomic disk-loss streams.
use super::{qemu::TimerSetup, Error};
use seedbox_machine::definition::BackendPolicy;
use seedbox_machine::machine::disk::Store;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PolicyState {
    pub name: String,
    pub configuration: BackendPolicy,
}
impl PolicyState {
    pub fn new(name: &str, configuration: &BackendPolicy) -> Result<Self, Error> {
        if name.contains('\0') {
            return Err(Error(
                "TCG backend policy machine name contains NUL; supply a name representable by QMP"
                    .into(),
            ));
        }
        configuration
            .validate()
            .map_err(|e| Error(format!("TCG backend policy: {e}")))?;
        Ok(Self {
            name: name.to_owned(),
            configuration: configuration.clone(),
        })
    }
    pub fn timer_setup(&self, boot: u64) -> TimerSetup {
        TimerSetup {
            seed: self.configuration.seed,
            jitter: self.configuration.timer_jitter_ns,
            domain: if boot == 0 {
                format!("timer-jitter/{}", self.name)
            } else {
                format!("timer-jitter/{}/{boot}", self.name)
            },
        }
    }
}
/// Restored names and configurations pass the same checks as factory input.
impl seedbox_machine::snapshot::Record for PolicyState {
    const MIN: u64 = <String as seedbox_machine::snapshot::Record>::MIN
        + <BackendPolicy as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            name,
            configuration,
        } = self;
        name.encode(w)?;
        configuration.encode(w)
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        let name = String::decode(r)?;
        let configuration = BackendPolicy::decode(r)?;
        Self::new(&name, &configuration)
            .map_err(|_| seedbox_machine::snapshot::CodecError::Invalid("backend policy"))
    }
}
pub(super) fn validate_authority(
    retained: Option<&PolicyState>,
    name: &str,
    expected: Option<&BackendPolicy>,
) -> Result<(), Error> {
    match (retained, expected) {
        (None, None) => Ok(()),
        (Some(p), Some(e)) if p.name == name && p.configuration == *e => Ok(()),
        _ => Err(Error("TCG snapshot backend policy differs from retained factory authority; restore with the original named configured policy (absence and an explicit default are distinct)".into())),
    }
}
pub(super) fn disk_digest(store: &Store, torn: bool) -> Result<String, Error> {
    store
        .state_digest(torn)
        .map_err(|e| Error(format!("TCG disk evidence: {e:?}")))
}
pub(super) fn lose(
    store: &Store,
    policy: Option<&PolicyState>,
    boot: u64,
) -> Result<(Store, u64), Error> {
    let torn = policy
        .filter(|p| p.configuration.torn_writes)
        .map(|p| (p.configuration.seed, p.name.as_str()));
    store
        .crash(torn, boot)
        .map_err(|e| Error(format!("TCG {e}")))
}
