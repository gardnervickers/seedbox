//! One serialized worker connection for playback and semantic interventions.
//! The catalogue lock is never held during transport or journal I/O.
use seedbox_api::{ControlCommand, ControlRequest, ControlState, Intervention, PlaybackStatus};
use seedbox_application::host_control::{Client, ClientError};
use seedbox_artifacts::interventions::Journal;
use seedbox_engine_protocol::host_control::{Command, Reply};
use std::{path::PathBuf, time::Duration};

const ATTEMPT: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("worker control connection is retired")]
    Retired,
    #[error("checkpoint capture is pending")]
    Pending,
    #[error(transparent)]
    Transport(#[from] ClientError),
    #[error("{0}")]
    Fatal(String),
}

struct FaultTransaction {
    request: ControlRequest,
    /// Set only after the exact proposal has been durably retained.
    retained: Option<Intervention>,
}

pub(crate) struct Worker {
    retired: bool,
    run: PathBuf,
    client: Option<Client>,
    journal: Option<Journal>,
    fault: Option<FaultTransaction>,
}

impl Worker {
    pub fn new(run: PathBuf) -> Self {
        Self {
            retired: false,
            run,
            client: None,
            journal: None,
            fault: None,
        }
    }

    pub(crate) fn retire(&mut self) {
        self.retired = true;
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.retired
    }

    fn client(&mut self) -> Result<&mut Client, ClientError> {
        if self.client.is_none() {
            self.client = Some(Client::for_run(&self.run)?);
        }
        self.client
            .as_mut()
            .ok_or_else(|| ClientError::Protocol("missing worker connection".into()))
    }

    pub fn apply(&mut self, request: &ControlRequest) -> Result<ControlState, Error> {
        if self.retired {
            return Err(Error::Retired);
        }
        let client = self.client()?;
        // Resolve a timed-out read before any mutation can overtake it.
        if client
            .pending()
            .is_some_and(|request| request.command == Command::Status)
        {
            client.execute(Command::Status, ATTEMPT)?;
        }
        if let ControlCommand::InjectFault { action } = &request.command {
            let result = self.inject(request, action.clone());
            if matches!(result, Err(Error::Transport(ClientError::Unsupported))) {
                self.fault = None;
            }
            return result;
        }
        if self.fault.is_some() {
            return Err(Error::Fatal(
                "another command overtook a pending fault".into(),
            ));
        }
        let command = match request.command {
            ControlCommand::SetRate { rate } => Command::SetRate { rate },
            ControlCommand::PauseBeforeNextFault { rate } => Command::PauseBeforeNextFault { rate },
            ControlCommand::Step => Command::Step,
            ControlCommand::Checkpoint => Command::Checkpoint {
                request_id: request.request_id.clone(),
            },
            ControlCommand::InjectFault { .. } => {
                return Err(Error::Fatal("invalid control dispatch".into()))
            }
        };
        let reply = self.client()?.execute(command, ATTEMPT)?;
        if request.command == ControlCommand::Checkpoint && reply.error.is_none() {
            let checkpoint = reply
                .checkpoint
                .as_ref()
                .ok_or_else(|| Error::Fatal("worker omitted checkpoint status".into()))?;
            if checkpoint.request_id != request.request_id {
                return Err(Error::Fatal(
                    "worker returned a different checkpoint".into(),
                ));
            }
            if checkpoint.state == seedbox_engine_protocol::host_control::CheckpointState::Requested
            {
                return Err(Error::Pending);
            }
            // No later mutation can overtake this request before its receipt.
            // Freeze the prefix now: the source may accept another fault at
            // this same paused boundary after capture has been acknowledged.
            let mut history =
                seedbox_artifacts::interventions::read(&self.run.join("interventions.json"))
                    .map_err(|e| Error::Fatal(e.to_string()))?;
            history.entries.retain(|entry| {
                entry.position.boundary_index <= checkpoint.position.boundary_index
            });
            let bytes = history.encode().map_err(Error::Fatal)?;
            std::fs::write(
                self.run.join(format!(
                    "checkpoint-{}-interventions.json",
                    request.request_id
                )),
                bytes,
            )
            .map_err(|e| Error::Fatal(format!("checkpoint intervention history: {e}")))?;
        }
        Ok(receipt(reply))
    }

    fn inject(
        &mut self,
        request: &ControlRequest,
        action: seedbox_api::FaultAction,
    ) -> Result<ControlState, Error> {
        if let Some(transaction) = &self.fault {
            if &transaction.request != request {
                return Err(Error::Fatal(
                    "fault request changed during admission".into(),
                ));
            }
        } else {
            if self.journal.is_none() {
                self.journal = Some(
                    Journal::open(&self.run.join("interventions.json"))
                        .map_err(|e| Error::Fatal(e.to_string()))?,
                );
            }
            self.fault = Some(FaultTransaction {
                request: request.clone(),
                retained: None,
            });
        }
        if self.fault.as_ref().is_some_and(|t| t.retained.is_none()) {
            let reply = self.client()?.execute(
                Command::PrepareFault {
                    request_id: request.request_id.clone(),
                    action: action.clone(),
                    timeout_ns: seedbox_control::PROCESS_EFFECT_TIMEOUT_NS,
                },
                ATTEMPT,
            )?;
            if reply.error.is_some() {
                self.fault = None;
                return Ok(receipt(reply));
            }
            let record = reply
                .intervention
                .ok_or_else(|| Error::Fatal("worker omitted its fault proposal".into()))?;
            if record.request_id != request.request_id
                || record.plan.action != action
                || record.position != reply.position
                || record.plan.at_ns != reply.position.simulated_time_ns
                || record
                    .plan
                    .at_ns
                    .checked_add(seedbox_control::PROCESS_EFFECT_TIMEOUT_NS)
                    != Some(record.plan.deadline_ns)
            {
                return Err(Error::Fatal(
                    "worker fault proposal differs from the accepted request".into(),
                ));
            }
            self.journal
                .as_mut()
                .ok_or_else(|| Error::Fatal("missing intervention journal".into()))?
                .append(record.clone())
                .map_err(|e| Error::Fatal(e.to_string()))?;
            self.fault
                .as_mut()
                .ok_or_else(|| Error::Fatal("missing fault transaction".into()))?
                .retained = Some(record);
        }
        let committed = self.client()?.execute(
            Command::CommitFault {
                request_id: request.request_id.clone(),
            },
            ATTEMPT,
        );
        let reply = match committed {
            Ok(reply) => reply,
            Err(
                error @ (ClientError::Unsupported
                | ClientError::Protocol(_)
                | ClientError::Exhausted),
            ) => {
                return Err(Error::Fatal(format!(
                    "cannot commit retained intervention: {error}"
                )));
            }
            Err(error) => return Err(error.into()),
        };
        let retained = self
            .fault
            .as_ref()
            .and_then(|t| t.retained.as_ref())
            .ok_or_else(|| Error::Fatal("fault commit preceded retention".into()))?;
        if reply.error.is_some() || reply.intervention.as_ref() != Some(retained) {
            return Err(Error::Fatal(format!(
                "worker did not acknowledge the retained fault: {:?}",
                reply.error
            )));
        }
        self.fault = None;
        Ok(receipt(reply))
    }

    pub fn inspect(&mut self) -> Result<PlaybackStatus, ClientError> {
        if self.retired {
            return Err(ClientError::Protocol(
                "worker control connection is retired".into(),
            ));
        }
        let reply = self.client()?.execute(Command::Status, ATTEMPT)?;
        if let Some(error) = reply.error {
            return Err(ClientError::Protocol(format!("status rejected: {error:?}")));
        }
        Ok(status(&reply))
    }
}

fn status(reply: &Reply) -> PlaybackStatus {
    PlaybackStatus {
        position: reply.position,
        rate: reply.rate,
        achieved_rate: reply.achieved_rate,
        stepping: reply.stepping,
        terminal: reply.terminal,
        application_ready: reply.application_ready,
    }
}

fn receipt(reply: Reply) -> ControlState {
    ControlState::Acknowledged {
        status: status(&reply),
        error: reply.error,
        intervention: reply.intervention.map(Box::new),
        detail: reply.detail,
        checkpoint: reply.checkpoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retired_connection_rejects_operations_before_opening_transport_or_journals() {
        let mut worker = Worker::new(PathBuf::from("/unused-retired-worker"));
        worker.retire();
        assert!(worker.is_retired());
        assert!(matches!(
            worker.apply(&ControlRequest {
                request_id: "stale".into(),
                command: ControlCommand::Checkpoint
            }),
            Err(Error::Retired)
        ));
        assert!(worker.inspect().is_err());
        assert!(worker.client.is_none());
        assert!(worker.journal.is_none());
    }
}
