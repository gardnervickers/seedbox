#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
exec 9>"$root/measurement.lock"
flock -n 9 || { printf 'Measurement lock busy\n' >&2; exit 73; }
mkdir -p "$root/runs/w6"
task_run="$(mktemp -d "$root/runs/w6/native.XXXXXX")"
cd "$task_run"
uname -a
cc --version | head -1
printf 'Native fixture baseline; no world seed or simulated backend involved\n'
src="$root/prior-art/hermit/tests/c/epoll_pwait_zero_timeout_progress.c"
cc -std=c11 -O2 -g -pthread -D_GNU_SOURCE -Wall -Wextra -Werror "$src" -o epoll-progress
printf 'mode=polling\n'
timeout --kill-after=1s 5s ./epoll-progress
printf 'mode=blocking\n'
timeout --kill-after=1s 5s ./epoll-progress blocking
sed 's/if (write(g_event_fd/if (0 \&\& write(g_event_fd/g' "$src" > no-producer-write.c
cc -std=c11 -O2 -g -pthread -D_GNU_SOURCE -Wall -Wextra -Werror no-producer-write.c -o no-producer-write
set +e
timeout --kill-after=1s 2s ./no-producer-write
mutant_status=$?
set -e
printf 'negative_control=no-producer-write exit=%s expected=124\n' "$mutant_status"
test "$mutant_status" -eq 124
sha256sum "$src" no-producer-write.c epoll-progress no-producer-write "$root/env/flake.lock"
