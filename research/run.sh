#!/usr/bin/env bash
set -u
task_root="$(cd "$(dirname "$0")" && pwd)"
task_slice="${1:-}"
case "$task_slice" in
  w1|w2|w3|w4|w5a|w5b|w6) shift ;;
  *) printf 'Usage: run.sh w1|w2|w3|w4|w5a|w5b|w6 command [args...]\n' >&2; exit 2 ;;
esac
if [ "$#" -eq 0 ]; then
  printf 'A command is required.\n' >&2
  exit 2
fi
mkdir -p "$task_root/runs/$task_slice"
task_run="$(mktemp -d "$task_root/runs/$task_slice/run.XXXXXX")"
printf '%q ' "$@" > "$task_run/command.txt"
printf '\n' >> "$task_run/command.txt"
pwd > "$task_run/cwd.txt"
"$@" > "$task_run/stdout.txt" 2> "$task_run/stderr.txt"
task_result=$?
printf '%s\n' "$task_result" > "$task_run/exit-code.txt"
printf 'Evidence: %s\nExit code: %s\n' "$task_run" "$task_result"
exit "$task_result"
