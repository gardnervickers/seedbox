#!/usr/bin/env bash
set -euo pipefail
probe_root="$(cd "$(dirname "$0")" && pwd)"
research_root="$(cd "$probe_root/../.." && pwd)"
task_jdk="${1:?exact JDK home resolved from the shared project lock required}"
mkdir -p "$research_root/runs/w5b"
task_out="${2:-$(mktemp -d "$research_root/runs/w5b/java-fixture.XXXXXX")}"
mkdir -p "$task_out/classes"
"$task_jdk/bin/java" -version > "$task_out/java-version.txt" 2>&1
cat "$task_jdk/release" > "$task_out/jdk-release.txt"
"$task_jdk/bin/javac" --release 21 -d "$task_out/classes" "$probe_root/AdmissionProbe.java"
# Lightweight fixture heap differs explicitly from W4's 128 MiB benchmark heap.
task_flags=(-Xms32m -Xmx32m -XX:+UseG1GC -XX:ActiveProcessorCount=1 -XX:CICompilerCount=2 -XX:+TieredCompilation -XX:+UnlockExperimentalVMOptions -XX:-UseFastUnorderedTimeStamps)
printf '%s\n' "${task_flags[@]}" > "$task_out/flags.txt"
"$task_jdk/bin/java" "${task_flags[@]}" -XX:+PrintFlagsFinal -version > "$task_out/effective-flags.txt" 2>&1
for trial in 1 2; do
    "$task_jdk/bin/java" "${task_flags[@]}" -cp "$task_out/classes" AdmissionProbe --self-check > "$task_out/self-check-$trial.json"
    "$task_jdk/bin/java" "${task_flags[@]}" "-Xlog:jit+compilation=debug,gc,safepoint:file=$task_out/observe-42-$trial.vm.log" -cp "$task_out/classes" AdmissionProbe --observe 42 > "$task_out/observe-42-$trial.json"
done
cmp "$task_out/self-check-1.json" "$task_out/self-check-2.json"
"$task_jdk/bin/java" "${task_flags[@]}" "-Xlog:jit+compilation=debug,gc,safepoint:file=$task_out/observe-43.vm.log" -cp "$task_out/classes" AdmissionProbe --observe 43 > "$task_out/observe-43.json"
task_payload42="$(jq -r .payload_checksum "$task_out/observe-42-1.json")"
test "$task_payload42" = "$(jq -r .payload_checksum "$task_out/observe-42-2.json")"
test "$task_payload42" != "$(jq -r .payload_checksum "$task_out/observe-43.json")"
python3 - "$task_out" "$probe_root" "$research_root" "$task_jdk" <<'PY'
import hashlib, json, pathlib, platform, sys
out, source, research, jdk = map(pathlib.Path, sys.argv[1:])
observations = [json.loads((out / name).read_text()) for name in
                ['observe-42-1.json', 'observe-42-2.json', 'observe-43.json']]
assert observations[0]['payload_checksum'] == observations[1]['payload_checksum']
assert observations[0]['payload_checksum'] != observations[2]['payload_checksum']
for name in ['observe-42-1.vm.log', 'observe-42-2.vm.log', 'observe-43.vm.log']:
    log = (out / name).read_text()
    assert 'AdmissionProbe::observe' in log and 'GC(' in log and 'Safepoint' in log
saved = json.loads((out / 'self-check-1.json').read_text())
assert saved['external_seed_does_not_reset_saved_generator']
assert saved['explicit_workload_seed_sensitive']
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
manifest = {
    'scope': 'portable_fixture_only_not_Linux_x86_or_VM_admission',
    'host_system': platform.system(), 'host_machine': platform.machine(),
    'jdk_home': str(jdk), 'shared_env_lock_sha256': digest(research / 'env/flake.lock'),
    'source_sha256': {name: digest(source / name) for name in ['AdmissionProbe.java', 'check.sh']},
    'checks': {'fresh_seed42_payload_equal': True, 'seed43_payload_differs': True,
               'saved_Random_ignores_external_seed_property': True,
               'each_observe_log_contains_JIT_GC_safepoint': True},
    'artifacts': {str(p.relative_to(out)): digest(p) for p in sorted(out.rglob('*'))
                  if p.is_file() and p.name != 'fixture-manifest.json'},
}
(out / 'fixture-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY
echo 'PASS explicit workload seed and saved-Random negative; runtime observations are not Linux/x86/VM admission'
echo "$task_out"
