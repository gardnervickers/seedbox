//! Oracles within one server session, each against the shared baseline.
use crate::session::{err, within, Session, REAL_TIME};
use crate::trace::{fault_identity, later_faults, same_output, suffix, Trace};
use crate::{input, Outcome, SECOND};
use futures_util::StreamExt;
use seedbox_api::{
    ComparisonStatus, ControlReceipt, ControlRequest, ControlState, DeriveRequest, LogDelivery,
    LogEntry, PlaybackRate, SimulationId,
};
use seedbox_client::Client;
use serde_json::json;
use std::{collections::BTreeMap, num::NonZeroU32};

impl Session {
    // ---- PerfSettingInvariance ----------------------------------------------------

    /// Playback rate, pauses and observers are host settings: a slowed run with
    /// fast followers, and a run paused twice with stalled and disconnecting
    /// subscribers, must both reproduce the unobserved unlimited baseline.
    pub async fn perf_settings(&self, baseline: &SimulationId) -> Outcome {
        let used = self.machine_workers(baseline).await?;
        if Some(used) != self.workers {
            return Err(format!(
                "{baseline}: {used} machine workers, not {:?}",
                self.workers
            ));
        }
        // Slow playback for the first simulated seconds, with fast followers.
        let slow = self.submit(&self.document, true).await?;
        self.live(&slow).await?;
        let mut followers = Vec::new();
        let logs = self.client.all_logs(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(logs.for_each(|_| async {})));
        let events = self.client.events(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(events.for_each(|_| async {})));
        let frames = self.client.frames(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(frames.for_each(|_| async {})));
        let tenth = PlaybackRate::Limited {
            numerator: NonZeroU32::MIN,
            denominator: NonZeroU32::new(10).unwrap(),
        };
        self.rate(&slow, tenth).await?;
        if !self.reach(&slow, 3 * SECOND).await? {
            return Err(format!("{slow} ended before 3 s"));
        }
        self.rate(&slow, PlaybackRate::Unlimited).await?;
        self.complete(&slow).await?;
        for follower in followers {
            follower.abort();
        }
        self.same_results(baseline, &slow)
            .await
            .map_err(|e| format!("rate 1/10 with fast followers: {e}"))?;

        // Paused twice, with stalled and disconnecting subscribers.
        let paused = self.submit(&self.document, true).await?;
        self.live(&paused).await?;
        let stalled = (
            self.client.all_logs(&paused).await.map_err(err)?,
            self.client.logs(&paused, "server").await.map_err(err)?,
            self.client.events(&paused).await.map_err(err)?,
            self.client.frames(&paused).await.map_err(err)?,
        );
        let quitter = self.client.frames(&paused).await.map_err(err)?;
        let quitter = tokio::spawn(quitter.take(20).for_each(|_| async {}));
        let logs_quitter = self.client.all_logs(&paused).await.map_err(err)?;
        let logs_quitter = tokio::spawn(logs_quitter.take(5).for_each(|_| async {}));
        // The server samples positions every 250 ms; in real time the run is
        // paused near each target rather than finishing between samples.
        self.rate(&paused, REAL_TIME).await?;
        for at in [6, 12] {
            if !self.reach(&paused, at * SECOND).await? {
                return Err(format!("{paused} ended before {at} s"));
            }
            self.rate(&paused, PlaybackRate::Paused).await?;
            // The pause holds while observers read it: the position stays put.
            let held = self.client.playback(&paused).await.map_err(err)?.state;
            for _ in 0..20 {
                self.client.world(&paused).await.map_err(err)?;
                let now = self.client.playback(&paused).await.map_err(err)?.state;
                if now.position != held.position {
                    return Err(format!(
                        "{paused} advanced while paused: {:?} -> {:?}",
                        held.position, now.position
                    ));
                }
            }
            let resume = if at == 6 {
                REAL_TIME
            } else {
                PlaybackRate::Unlimited
            };
            self.rate(&paused, resume).await?;
        }
        self.complete(&paused).await?;
        drop(stalled);
        quitter.abort();
        logs_quitter.abort();
        self.same_results(baseline, &paused)
            .await
            .map_err(|e| format!("paused twice with stalled subscribers: {e}"))
    }

    // ---- ConcurrentSimulationIsolation ----------------------------------------------

    /// A runs beside B, built from the same assets and seed, while B is resumed,
    /// faulted, paused, checkpointed, stopped and deleted. A must equal the solo
    /// baseline, and nothing done to B may appear in A.
    pub async fn concurrent_isolation(&self, baseline: &SimulationId) -> Outcome {
        let b = self.submit(&self.document, true).await?;
        self.live(&b).await?;
        // A plays in real time while B is controlled, then at unlimited rate.
        let a = self.submit(&self.document, true).await?;
        self.live(&a).await?;
        self.rate(&a, REAL_TIME).await?;
        let mut requests = Vec::new();
        let mut overlapped = true;
        self.rate(&b, REAL_TIME).await?;
        if !self.reach(&b, 2 * SECOND).await? {
            return Err(format!("{b} ended before 2 s"));
        }
        let fault = self
            .inject(
                &b,
                json!({"action": "partition", "handle": "b-only",
                       "left": ["worker"], "right": ["server", "oracle"]}),
            )
            .await?;
        requests.push(fault.request.request_id.clone());
        for rate in [PlaybackRate::Paused, REAL_TIME, PlaybackRate::Paused] {
            let receipt = self
                .control(&b, seedbox_api::ControlCommand::SetRate { rate })
                .await?;
            requests.push(receipt.request.request_id);
            overlapped &= !self.terminal(&a).await;
        }
        let checkpoint = self
            .control(&b, seedbox_api::ControlCommand::Checkpoint)
            .await?;
        requests.push(checkpoint.request.request_id);
        let b_world = self.client.world(&b).await.map_err(err)?;
        if !b_world
            .faults
            .iter()
            .any(|f| f.plan.as_ref().is_some_and(|p| p["handle"] == "b-only"))
        {
            return Err(format!("{b}: injected fault missing from its own world"));
        }
        overlapped &= !self.terminal(&a).await;
        self.client.stop(&b).await.map_err(err)?;
        self.client.delete(&b).await.map_err(err)?;
        overlapped &= !self.terminal(&a).await;
        if !overlapped {
            return Err(format!(
                "{a} finished before B's controls; lengthen the fixture"
            ));
        }
        self.rate(&a, PlaybackRate::Unlimited).await?;
        self.complete(&a).await?;
        self.same_results(baseline, &a).await?;
        for request in &requests {
            if self
                .client
                .control_receipt::<ControlReceipt>(&a, request)
                .await
                .is_ok()
            {
                return Err(format!("{a} reports B's control {request}"));
            }
        }
        let a_world = self.client.world(&a).await.map_err(err)?;
        let base_world = self.client.world(baseline).await.map_err(err)?;
        if fault_identity(&a_world.faults) != fault_identity(&base_world.faults) {
            return Err(format!(
                "{a} faults differ from the baseline: {:?}",
                a_world.faults
            ));
        }
        if !self.client.checkpoints(&a).await.map_err(err)?.is_empty() {
            return Err(format!("{a} lists B's checkpoint"));
        }
        let events = serde_json::to_string(&self.events(&a).await?).map_err(err)?;
        if events.contains(b.as_str()) || events.contains("b-only") {
            return Err(format!("{a} events mention B"));
        }
        same_output(&self.logs(baseline).await?, &self.logs(&a).await?)
            .map_err(|e| format!("{a} logs differ from the baseline: {e}"))
    }

    // ---- CheckpointTransparencyAndBranchIdentity ------------------------------------

    /// Capture at each oracle marker (pause, checkpoint, resume); the captured run
    /// must equal the baseline. Two branches from one checkpoint must match each
    /// other, and their output and later faults must equal the captured run's
    /// suffix.
    pub async fn checkpoint_transparency(&self, baseline: &SimulationId) -> Outcome {
        let captured = self.submit(&self.document, true).await?;
        self.live(&captured).await?;
        let mut marks = self.client.logs(&captured, "oracle").await.map_err(err)?;
        // Real-time playback leaves the blocked child in place when the host pauses.
        self.rate(&captured, REAL_TIME).await?;
        let mut text = String::new();
        let mut checkpoints = Vec::new();
        while let Some(delivery) = marks.next().await {
            match delivery.map_err(err)? {
                LogDelivery::Entry {
                    entry: LogEntry::Bytes { bytes, .. },
                    ..
                } => text.push_str(&String::from_utf8_lossy(&bytes)),
                LogDelivery::Closed => break,
                _ => {}
            }
            while checkpoints.len() < text.matches("MARK ").count() {
                checkpoints.push(self.checkpoint(&captured).await?);
                self.rate(&captured, REAL_TIME).await?;
            }
            if text.contains("oracle worker digest") {
                break;
            }
        }
        drop(marks);
        if checkpoints.len() != 5 {
            return Err(format!(
                "{captured}: {} checkpoints for 5 markers",
                checkpoints.len()
            ));
        }
        self.finish(&captured).await?;
        self.complete(&captured).await?;
        self.same_results(baseline, &captured)
            .await
            .map_err(|e| format!("captured run: {e}"))?;
        let listed = self.client.checkpoints(&captured).await.map_err(err)?;
        for (name, _) in &checkpoints {
            if !listed.iter().any(|r| &r.request.request_id == name) {
                return Err(format!(
                    "{captured}: checkpoint {name} not listed: {listed:?}"
                ));
            }
        }
        // MARK 9: between the healed partition and the pending throttle and heal inputs.
        let (name, at) = &checkpoints[2];
        let document = self.checkpoint_document(&captured, name).await?;
        let first = self.submit(&document, false).await?;
        let second = self.submit(&document, false).await?;
        self.complete(&first).await?;
        self.complete(&second).await?;
        self.same_results(&first, &second)
            .await
            .map_err(|e| format!("branches from {name}: {e}"))?;
        let expected = suffix(self.logs(&captured).await?, *at);
        let source_faults = self.client.world(&captured).await.map_err(err)?.faults;
        for branch in [&first, &second] {
            same_output(&expected, &self.logs(branch).await?).map_err(|e| {
                format!("{branch} output differs from {captured} after {at} ns:\n{e}")
            })?;
            let branch_faults = self.client.world(branch).await.map_err(err)?.faults;
            if later_faults(&branch_faults, *at) != later_faults(&source_faults, *at) {
                return Err(format!(
                    "{branch} faults after {at} ns differ: {:?} vs {:?}",
                    later_faults(&branch_faults, *at),
                    later_faults(&source_faults, *at)
                ));
            }
        }
        Ok(())
    }

    // ---- EditedTimelinePrefix ---------------------------------------------------------

    /// Edited definitions (drop `slow`; move `unshape` earlier; add an input)
    /// must reproduce the baseline before their first changed input time, and
    /// the oracle's own actions must neither repeat nor disappear.
    pub async fn edited_timeline_prefix(&self, baseline: &SimulationId) -> Outcome {
        let source = self.client.describe(baseline).await.map_err(err)?;
        let dropped = self.created(
            self.client
                .derive(
                    baseline,
                    &DeriveRequest {
                        request_id: Client::request_id().map_err(err)?,
                        source_created_at_unix_ms: source.metadata.created_at_unix_ms,
                        included: vec!["program:lossy".into(), "program:unshape".into()],
                        start_paused: false,
                    },
                )
                .await
                .map_err(err)?
                .metadata
                .id,
        );
        let mut moved = source.clone();
        for input in &mut moved.spec.program {
            if input.id == "unshape" {
                input.at_ns = 12 * SECOND;
            }
        }
        moved.spec.program.sort_by_key(|input| input.at_ns);
        let mut added = source.clone();
        added.spec.program.push(input(
            "server-slow",
            12 * SECOND,
            json!({"action": "throttle", "handle": "server-slow", "machine": "server",
                   "factor_permille": 2500}),
        ));
        added.spec.program.sort_by_key(|input| input.at_ns);
        let moved = self
            .submit(&serde_json::to_value(&moved).map_err(err)?, false)
            .await?;
        let added = self
            .submit(&serde_json::to_value(&added).map_err(err)?, false)
            .await?;
        let base = Trace::read(self, baseline).await?;
        for (edited, changed_at, present, absent) in [
            (&dropped, 10 * SECOND, "unshape", "slow"),
            (&moved, 12 * SECOND, "unshape", ""),
            (&added, 12 * SECOND, "server-slow", ""),
        ] {
            self.complete(edited).await?;
            // The edit must change the run, or the prefix check proves nothing.
            let comparison = self.client.compare(baseline, edited).await.map_err(err)?;
            if comparison.status == ComparisonStatus::Match {
                return Err(format!("{edited}: edit left the whole run unchanged"));
            }
            let trace = Trace::read(self, edited).await?;
            base.same_prefix(&trace, changed_at)
                .map_err(|e| format!("{edited} (first change at {changed_at} ns): {e}"))?;
            let keys: Vec<_> = trace
                .faults
                .iter()
                .filter_map(|f| f["inputKey"].as_str())
                .collect();
            if !keys.contains(&present) || keys.contains(&absent) {
                return Err(format!("{edited}: edit not applied; inputs {keys:?}"));
            }
            if trace.oracle_actions() != base.oracle_actions() {
                return Err(format!(
                    "{edited}: oracle actions {:?} vs baseline {:?}",
                    trace.oracle_actions(),
                    base.oracle_actions()
                ));
            }
        }
        Ok(())
    }

    // ---- PreviewPurity ----------------------------------------------------------------

    /// Hundreds of fault previews and world/timeline reads while paused with
    /// seeded loss and jitter active must not change any later admission,
    /// packet decision or result.
    pub async fn preview_purity(&self, baseline: &SimulationId) -> Outcome {
        let id = self.submit(&self.document, true).await?;
        self.live(&id).await?;
        self.rate(&id, REAL_TIME).await?;
        if !self.reach(&id, 5 * SECOND + SECOND / 2).await? {
            return Err(format!("{id} ended before 5.5 s"));
        }
        self.rate(&id, PlaybackRate::Paused).await?;
        let world = self.client.world(&id).await.map_err(err)?;
        if !world
            .faults
            .iter()
            .any(|f| f.input_key.as_deref() == Some("lossy") && f.active)
        {
            return Err(format!("{id}: lossy shape not active at the pause"));
        }
        let connection = json!({"from": "worker", "to": "server", "src_addr": "10.0.0.9",
            "dst_addr": "10.0.0.8", "src_port": 40000, "dst_port": 7000, "incarnation": 99});
        let actions = [
            json!({"action": "shape", "handle": "p-shape", "from": ["worker"], "to": ["server"],
                   "latency_ns": 5_000_000, "jitter_ns": 1_000_000, "loss_ppm": 500_000}),
            json!({"action": "partition", "handle": "p-cut", "left": ["worker"],
                   "right": ["server", "oracle"]}),
            json!({"action": "heal", "handle": "lossy"}),
            json!({"action": "heal", "handle": "absent"}),
            json!({"action": "drop_connection", "handle": "p-drop", "connection": connection}),
            json!({"action": "clock_skew", "handle": "p-skew", "machine": "server",
                   "max_offset_ns": 5_000_000_000u64}),
            json!({"action": "clock_drift", "handle": "p-drift", "machine": "worker",
                   "max_rate_ppb": 100_000_000}),
            json!({"action": "throttle", "handle": "p-slow", "machine": "server",
                   "factor_permille": 10_000}),
            json!({"action": "crash", "machine": "worker"}),
        ];
        let mut previews = 0;
        let mut explained = BTreeMap::<String, usize>::new();
        for round in 0..40 {
            for action in &actions {
                // Rejections (an unknown handle, a stale connection) are previews too.
                if self.client.preview_fault(&id, action).await.is_ok() {
                    *explained.entry(action["action"].to_string()).or_default() += 1;
                }
                previews += 1;
            }
            if round % 4 == 0 {
                self.client.world(&id).await.map_err(err)?;
                self.client.timeline(&id).await.map_err(err)?;
                self.client.fault_types(&id).await.map_err(err)?;
            }
        }
        // Every kind except the stale connection must be explained.
        if explained.len() < 7 {
            return Err(format!("{id}: previews mostly rejected: {explained:?}"));
        }
        let after = self.client.world(&id).await.map_err(err)?;
        if after.revision != world.revision || after.faults != world.faults {
            return Err(format!(
                "{id}: {previews} previews changed the paused world"
            ));
        }
        self.rate(&id, PlaybackRate::Unlimited).await?;
        self.complete(&id).await?;
        self.same_results(baseline, &id).await?;
        let faults = fault_identity(&self.client.world(&id).await.map_err(err)?.faults);
        let base = fault_identity(&self.client.world(baseline).await.map_err(err)?.faults);
        if faults != base {
            return Err(format!(
                "{id}: later fault IDs differ: {faults:?} vs {base:?}"
            ));
        }
        Ok(())
    }

    // ---- AcceptedFaultSurvivesDisconnect ----------------------------------------------

    /// Send one control over a raw connection and return it unread.
    fn post_control(
        &self,
        id: &SimulationId,
        request: &ControlRequest,
    ) -> Result<std::os::unix::net::UnixStream, String> {
        use std::io::Write;
        let body = serde_json::to_vec(request).map_err(err)?;
        let mut stream =
            std::os::unix::net::UnixStream::connect(self.state.join("server.sock")).map_err(err)?;
        write!(
            stream,
            "POST /v1/simulations/{id}/controls HTTP/1.1\r\nHost: seedbox.local\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .map_err(err)?;
        stream.write_all(&body).map_err(err)?;
        stream.flush().map_err(err)?;
        Ok(stream)
    }

    /// Keyed faults whose HTTP clients disconnect before reading the receipt,
    /// once right after writing (perhaps before acceptance) and once after
    /// acceptance: retrying each key must leave exactly one admission, the
    /// partition must drop packets, and the run must replay.
    pub async fn accepted_fault_survives_disconnect(&self, baseline: &SimulationId) -> Outcome {
        let id = self.submit(&self.document, true).await?;
        self.live(&id).await?;
        // Real time: at unlimited rate the rest of the run can end before the
        // retried partition is admitted, or within the second it must hold.
        self.rate(&id, REAL_TIME).await?;
        if !self.reach(&id, 2 * SECOND).await? {
            return Err(format!("{id} ended before 2 s"));
        }
        let partition = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command: seedbox_api::ControlCommand::InjectFault {
                action: serde_json::from_value(json!({"action": "partition",
                    "handle": "dropped-client", "left": ["worker"], "right": ["server"]}))
                .map_err(err)?,
            },
        };
        // Close right after writing: the server may or may not have accepted
        // it, so the client retries the same key, which admits it exactly once.
        drop(self.post_control(&id, &partition)?);
        let first: ControlReceipt = self
            .client
            .request_control(&id, &partition)
            .await
            .map_err(err)?;
        let receipt = self.acknowledged(&id, &partition.request_id).await?;
        // Retrying an acknowledged key returns that receipt and admits nothing new.
        let retried: ControlReceipt = self
            .client
            .request_control(&id, &partition)
            .await
            .map_err(err)?;
        if retried != receipt || first.request != receipt.request {
            return Err(format!(
                "{id}: retry changed the receipt: {retried:?} vs {receipt:?}"
            ));
        }
        let at = self.client.playback(&id).await.map_err(err)?.state.position;
        if !self.reach(&id, at.simulated_time_ns + SECOND).await? {
            return Err(format!("{id} ended while partitioned"));
        }
        // The heal's client disconnects after acceptance, before the response.
        let heal = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command: seedbox_api::ControlCommand::InjectFault {
                action: serde_json::from_value(
                    json!({"action": "heal", "handle": "dropped-client"}),
                )
                .map_err(err)?,
            },
        };
        let connection = self.post_control(&id, &heal)?;
        within(
            60,
            || format!("{id}: heal not accepted on an open connection"),
            async {
                while self
                    .client
                    .control_receipt::<ControlReceipt>(&id, &heal.request_id)
                    .await
                    .is_err()
                {}
                Ok(())
            },
        )
        .await?;
        drop(connection);
        self.acknowledged(&id, &heal.request_id).await?;
        let retried: ControlReceipt = self.client.request_control(&id, &heal).await.map_err(err)?;
        if !matches!(
            retried.state,
            ControlState::Acknowledged { error: None, .. }
        ) {
            return Err(format!("{id}: heal retry: {retried:?}"));
        }
        self.complete(&id).await?;
        let world = self.client.world(&id).await.map_err(err)?;
        let admitted: Vec<_> = world
            .faults
            .iter()
            .filter(|f| {
                f.action == "partition"
                    && f.plan
                        .as_ref()
                        .is_some_and(|p| p["handle"] == "dropped-client")
            })
            .collect();
        if admitted.len() != 1 {
            return Err(format!(
                "{id}: {} admissions of one keyed partition: {:?}",
                admitted.len(),
                world.faults
            ));
        }
        let interventions = self
            .client
            .timeline(&id)
            .await
            .map_err(err)?
            .entries
            .iter()
            .filter(|e| serde_json::to_string(e).is_ok_and(|t| t.contains("dropped-client")))
            .count();
        // The partition and its heal.
        if interventions != 2 {
            return Err(format!(
                "{id}: {interventions} timeline entries for the keyed fault"
            ));
        }
        let dropped = |world: &seedbox_api::WorldState| -> u64 {
            world
                .links
                .iter()
                .filter(|l| {
                    (l.from == "worker" && l.to == "server")
                        || (l.from == "server" && l.to == "worker")
                })
                .map(|l| l.counters.partition_dropped)
                .sum()
        };
        let base = self.client.world(baseline).await.map_err(err)?;
        if dropped(&world) <= dropped(&base) {
            return Err(format!(
                "{id}: the keyed partition dropped no packets ({} vs baseline {})",
                dropped(&world),
                dropped(&base)
            ));
        }
        // Exit status 0 is MATCH.
        self.cli(&["replay", id.as_str()]).map(drop)
    }
}
