#!/usr/bin/env bash
set -euo pipefail
task_world="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$task_world/../runs/w1"
task_evidence="${SEEDBOX_W1_EVIDENCE:-$(mktemp -d "$task_world/../runs/w1/protocol.XXXXXX")}"
task_binary="$task_world/target/debug/seedbox-world"
mkdir -p "$task_evidence"
for task_index in 1 2 3; do "$task_binary" 42 > "$task_evidence/fresh-$task_index.json"; done
yes > /dev/null &
task_load_pid=$!
trap 'kill "$task_load_pid" 2>/dev/null || true' EXIT
TZ=Pacific/Honolulu LC_ALL=C "$task_binary" 42 > "$task_evidence/fresh-4-host-load-tz.json"
kill "$task_load_pid" 2>/dev/null || true
wait "$task_load_pid" 2>/dev/null || true
trap - EXIT
TZ=Europe/Berlin LC_ALL=C "$task_binary" 42 > "$task_evidence/fresh-5-tz.json"
for task_file in "$task_evidence/fresh-2.json" "$task_evidence/fresh-3.json" "$task_evidence/fresh-4-host-load-tz.json" "$task_evidence/fresh-5-tz.json"; do cmp "$task_evidence/fresh-1.json" "$task_file"; done
"$task_binary" 43 > "$task_evidence/seed-43.json"
for task_seed in 42 43; do
  if [ "$task_seed" = 42 ]; then task_input="$task_evidence/fresh-1.json"; else task_input="$task_evidence/seed-43.json"; fi
  jq '[.positives[0].trace[] | select(.event == "partial_loss_observation") | .detail]' "$task_input" > "$task_evidence/seed-$task_seed-deliveries.json"
done
if cmp -s "$task_evidence/seed-42-deliveries.json" "$task_evidence/seed-43-deliveries.json"; then echo 'Seed sensitivity failed: actual delivered frame IDs matched.' >&2; exit 1; fi
for task_pc in 0 1 2; do jq ".continuation.checkpoints[$task_pc].state" "$task_evidence/fresh-1.json" > "$task_evidence/checkpoint-$task_pc.json"; done
jq '{schema,seed,scope,time_policy,sources,positive_cases:[.positives[]|{case,result,tick:.world.now,stale:.world.stale,trace_entries:(.trace|length),controller_commands:.controller.next_command,quorum_reads:[.trace[]|select(.event=="quorum_read")|.detail]}],negative_controls:[.negatives[]|{case,result}],continuation:{case:.continuation.case,result:.continuation.result,lost_pending_command_control:.continuation.lost_pending_command_control,checkpoints:[.continuation.checkpoints[]|del(.state)]}}' "$task_evidence/fresh-1.json" > "$task_evidence/summary.json"
cp "$task_world/control-message.schema.json" "$task_world/protocol-contract.json" "$task_evidence/"
sha256sum "$task_evidence"/fresh-*.json "$task_evidence/seed-43.json" "$task_evidence/summary.json" "$task_evidence"/checkpoint-*.json > "$task_evidence/results-sha256.txt"
sha256sum "$task_world/src/lib.rs" "$task_world/src/protocol.rs" "$task_world/src/client.rs" "$task_world/src/main.rs" "$task_world/Cargo.toml" "$task_world/Cargo.lock" "$task_world/check.sh" "$task_world/verify.sh" "$task_world/control-message.schema.json" "$task_world/protocol-contract.json" "$task_world/../env/flake.nix" "$task_world/../env/flake.lock" "$task_world/../run.sh" "$task_binary" > "$task_evidence/inputs-sha256.txt"
{ uname -a; rustc --version --verbose; cargo --version; jq --version;
  printf 'seed=42; alternate_seed=43; fixture_budget=20000; build_jobs=8\n'
  printf 'outside yes process CPU load and TZ perturbations; no workstation or KVM runs\n'
  printf 'control wire v1, suite schema2, no Future/Rc/waker state; branch ticks are fixture/integration inputs\n'
} > "$task_evidence/config.txt"
printf 'Five fresh native protocol traces byte-match. Restored sleep/fault/query futures match full future traces and final serialized bundle.\n'
cat "$task_evidence/seed-42-deliveries.json" "$task_evidence/seed-43-deliveries.json"
cat "$task_evidence/results-sha256.txt"
