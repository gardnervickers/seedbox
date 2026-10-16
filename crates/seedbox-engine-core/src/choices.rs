//! Structured choices. A committed choice request gets its occurrence within
//! its machine, incarnation, producer session and site; an armed choice input
//! selects it, or a branch-aware draw keyed by that identity does. The reply
//! goes back over the management channel one causal lookahead later.
use seedbox_engine_protocol::choice::{reply_line, ChoiceEvidence, ChoiceRecord, MAX_CHOICES};
use seedbox_engine_protocol::input_program::ChoiceOverride;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type Identity = (String, u32, u32, String);

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choices {
    /// Requests so far by machine, incarnation, session and site.
    #[serde(with = "crate::checkpoint::ordered_map")]
    occurrences: BTreeMap<Identity, u64>,
    /// Due choice inputs not yet consumed, in document order.
    overrides: Vec<(String, ChoiceOverride)>,
    evidence: ChoiceEvidence,
}

/// What answering a request decided.
pub struct Answer {
    pub reply_at: u64,
    /// The management line to deliver then.
    pub reply: Vec<u8>,
    pub chosen: String,
    /// The choice input that selected it, if one did.
    pub input_key: Option<String>,
}

/// A committed choice request.
pub struct Request<'a> {
    pub machine: &'a str,
    pub incarnation: u32,
    pub session: u32,
    pub site: &'a str,
    pub alternatives: &'a [String],
    pub at: u64,
}

impl Choices {
    pub fn arm(&mut self, key: String, choice: ChoiceOverride) {
        self.overrides.push((key, choice));
    }
    /// Select an alternative and return when and what to reply.
    pub fn request(
        &mut self,
        request: Request<'_>,
        lookahead: u64,
        run_seed: u64,
        branch_identity: Option<&str>,
    ) -> Result<Answer, String> {
        let Request {
            machine,
            incarnation,
            session,
            site,
            alternatives,
            at,
        } = request;
        let count = self
            .occurrences
            .entry((machine.into(), incarnation, session, site.into()))
            .or_default();
        *count = count.checked_add(1).ok_or("choice occurrence overflow")?;
        let occurrence = *count;
        let matched = self.overrides.iter().position(|(_, o)| {
            o.machine == machine
                && o.site == site
                && o.occurrence == occurrence
                && o.incarnation.is_none_or(|i| i == incarnation)
                && o.session.is_none_or(|s| s == session)
                && o.select < alternatives.len()
        });
        let (selected, input_key) = match matched {
            Some(index) => {
                let (key, o) = self.overrides.remove(index);
                (o.select, Some(key))
            }
            None => {
                let domain =
                    format!("choice/{machine}/{incarnation}/{session}/{site}/{occurrence}");
                let mut rng = crate::rng::Rng::branch(run_seed, branch_identity, &domain);
                (rng.below(alternatives.len() as u64) as usize, None)
            }
        };
        let reply_at = at.checked_add(lookahead).ok_or("choice reply overflow")?;
        let chosen = alternatives[selected].clone();
        if self.evidence.records.len() < MAX_CHOICES {
            self.evidence.records.push(ChoiceRecord {
                machine: machine.into(),
                incarnation,
                session,
                site: site.into(),
                occurrence,
                alternatives: alternatives.len(),
                selected,
                chosen: chosen.clone(),
                requested_at_ns: at,
                reply_at_ns: reply_at,
                input_key: input_key.clone(),
            });
        } else {
            self.evidence.untracked += 1;
        }
        Ok(Answer {
            reply_at,
            reply: reply_line(session, site, occurrence, selected),
            chosen,
            input_key,
        })
    }
    /// None when the run made no choice, so unused choices add no evidence.
    pub fn evidence(&self) -> Option<&ChoiceEvidence> {
        (!self.evidence.records.is_empty() || self.evidence.untracked > 0).then_some(&self.evidence)
    }
    pub fn hash_into(&self, hash: &mut Sha256) -> Result<(), String> {
        if let Some(evidence) = self.evidence() {
            hash.update(b"seedbox-choices-v1\0");
            hash.update(serde_json::to_vec(evidence).map_err(|e| e.to_string())?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(choices: &mut Choices, session: u32, at: u64, seed: u64) -> (u64, Vec<u8>) {
        let answer = ask_answer(choices, session, at, seed);
        (answer.reply_at, answer.reply)
    }
    fn ask_answer(choices: &mut Choices, session: u32, at: u64, seed: u64) -> Answer {
        let alternatives: Vec<String> = (0..8).map(|i| format!("alt-{i}")).collect();
        let request = Request {
            machine: "a",
            incarnation: 0,
            session,
            site: "pick",
            alternatives: &alternatives,
            at,
        };
        choices.request(request, 10, seed, None).unwrap()
    }

    #[test]
    fn choices_number_occurrences_draw_by_identity_and_take_overrides_once() {
        let mut choices = Choices::default();
        let override_ = ChoiceOverride {
            machine: "a".into(),
            site: "pick".into(),
            occurrence: 2,
            incarnation: None,
            session: Some(1),
            select: 5,
        };
        choices.arm("force".into(), override_);
        let first = ask(&mut choices, 1, 3, 7);
        let second = ask(&mut choices, 1, 4, 7);
        let other_session = ask(&mut choices, 2, 4, 7);
        assert_eq!(first.0, 13);
        let records = &choices.evidence().unwrap().records;
        assert_eq!(
            records.iter().map(|r| r.occurrence).collect::<Vec<_>>(),
            [1, 2, 1]
        );
        assert_eq!(
            (records[1].selected, records[1].input_key.as_deref()),
            (5, Some("force"))
        );
        assert_eq!(
            String::from_utf8(second.1).unwrap(),
            "SBX_SDK1 1 {\"choice\":{\"index\":5,\"occurrence\":2,\"site\":\"pick\"}}\n"
        );
        assert!(choices.overrides.is_empty());
        // Draws depend on identity and seed, never on what else was asked.
        let mut fresh = Choices::default();
        assert_eq!(ask(&mut fresh, 2, 99, 7).1, other_session.1);
        let drawn: Vec<_> = (0..16)
            .map(|seed| ask(&mut Choices::default(), 1, 0, seed).1)
            .collect();
        assert!(drawn.iter().any(|reply| reply != &drawn[0]));
        let saved: Choices =
            serde_json::from_value(serde_json::to_value(&choices).unwrap()).unwrap();
        assert_eq!(saved.occurrences, choices.occurrences);
    }
}
