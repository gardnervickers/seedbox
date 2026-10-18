#!/bin/sh
# Usage: run-jcstress.sh SUITE TEST_REGEX [jcstress options...]
# JCSTRESS_VIRTUAL_CPUS (default 4) is the CPU count reported to jcstress; actors beyond the
# hardware CPUs are time-sliced (see oversubscribe.patch).
# Runs jcstress, then prints one "@@RESULT <test> <verdict>" line per test from its text report
# so callers can compare verdicts without the timing-dependent outcome counts.
suite="$1"
filter="$2"
shift 2
cd /tmp || exit 2
java -Djcstress.virtualCpus="${JCSTRESS_VIRTUAL_CPUS:-4}" -XX:ErrorFile=/tmp/hs_err_driver.log -jar "/jcstress/$suite.jar" -t "$filter" -r /tmp/results "$@" < /dev/null 2>&1
code=$?
for f in /tmp/hs_err*.log /tmp/results/*hs_err*; do
    [ -f "$f" ] || continue
    echo "@@HSERR $f"
    head -c 24000 "$f" | sed 's/^/@@| /'
    echo
done
echo "@@EXIT jcstress $code"
