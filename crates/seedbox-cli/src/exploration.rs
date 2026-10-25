//! Exploration commands whose interface is defined before any server carries
//! them out. Arguments are parsed and checked as the implementation will
//! check them, then each command refuses, before connecting to a server.
use seedbox_application::{Error, Result};

/// The refusal every exploration command and option gives for now, in the
/// server's words.
pub(super) fn not_implemented(name: &str) -> Error {
    Error::Setup(seedbox_engine_protocol::not_implemented(name))
}

/// What `seedbox explain --kind` selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum ExplainKind {
    /// Program inputs other than faults: anchors, reseeds, triggers and choices.
    Input,
    /// Faults and their effects.
    Fault,
    /// Trigger receipts.
    Trigger,
    /// Machine lifecycle transitions.
    Lifecycle,
    /// Machines reporting ready.
    Ready,
    /// Application exits.
    Exit,
    /// Playback and control requests.
    Control,
    /// Answered choices.
    Choice,
    /// SDK setup milestones.
    Setup,
    /// SDK events.
    Event,
    /// SDK state values.
    State,
    /// SDK state transitions.
    Transition,
    /// Property evaluations and violations.
    Property,
}
impl From<ExplainKind> for seedbox_api::ExplanationKind {
    fn from(kind: ExplainKind) -> Self {
        match kind {
            ExplainKind::Input => Self::Input,
            ExplainKind::Fault => Self::Fault,
            ExplainKind::Trigger => Self::Trigger,
            ExplainKind::Lifecycle => Self::Lifecycle,
            ExplainKind::Ready => Self::Ready,
            ExplainKind::Exit => Self::Exit,
            ExplainKind::Control => Self::Control,
            ExplainKind::Choice => Self::Choice,
            ExplainKind::Setup => Self::Setup,
            ExplainKind::Event => Self::Event,
            ExplainKind::State => Self::State,
            ExplainKind::Transition => Self::Transition,
            ExplainKind::Property => Self::Property,
        }
    }
}

/// Check a campaign's plan and profile, then refuse.
pub(super) fn campaign(
    plan: seedbox_api::CampaignPlan,
    fault_profile: Option<&std::path::Path>,
) -> Result<u8> {
    plan.validate().map_err(Error::Setup)?;
    if let Some(path) = fault_profile {
        super::faults::profile(path)?;
    }
    Err(not_implemented("seedbox campaign"))
}

/// Check an analysis request, then refuse.
pub(super) fn analyze(request: seedbox_api::AnalysisRequest) -> Result<u8> {
    request.validate().map_err(Error::Setup)?;
    Err(not_implemented("seedbox analyze"))
}
