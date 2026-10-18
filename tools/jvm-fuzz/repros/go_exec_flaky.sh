#!/bin/sh
# Go.exec (probes/go/gofz.go `exec`) under the progress translator fails in about 3% of runs;
# natively it passed 68 of 68 runs. Smaller programs (one child sending paced SIGUSR1 while
# goroutines spawn and spin; 15000 concurrent os/exec children with chosen exit statuses; the
# same clone(CLONE_VM|CLONE_VFORK|CLONE_PIDFD) + waitid(P_PIDFD) pattern in C with tgkill'd
# SIGURG) did not reproduce it, so the probe itself is the reproducer.
#
# Usage: go_exec_flaky.sh GOFZ DRTREE [RUNS]
#   GOFZ: gofz built as the corpus image builds it (CGO_ENABLED=0 go build -trimpath)
#   DRTREE: the translator tree mounted at /seedbox/progress
#
# Failures seen in about 300 runs (seeds 1 and 2, one CPU, 2 GiB):
#  - lost SIGUSR1 (4 runs). The child sent it with kill(2) and waits in read(0) for the ack;
#    neither process has it pending in the kernel, and a SIGUSR1 the parent then sends itself
#    does arrive:
#      FAIL sig child 1: signal 3 never arrived (self-signal afterwards arrives: yes; child
#      State: S (sleeping), Threads: 3, SigPnd: 0..0, ShdPnd: 0..0, ..., task 63 0x0, ...;
#      parent State: R (running), Threads: 10, SigPnd: 0..0, ShdPnd: 0..0, ...)
#  - a record child dies inside DynamoRIO early in its life (3 runs exit 255, 1 run exit 0
#    where 3 was chosen):
#      FAIL rec 110 exit 255, want 2 (started pid 338, waited pid 338, status 0xff00, output "",
#      stderr "<Application /w/gofz (2).  DynamoRIO internal crash at PC 0x0000f3dc5d84de37.
#      ... Received SIGBUS at pc 0x0000f3dc5d84de37 in thread 339 ...")
#  - one run hung until the 600 s timeout during the record children (exit 124).
# The debug translator (-debug) cannot run any Go program here: it stops at startup with
#   "DynamoRIO debug check failure: core/utils.c:593 lock != &thread_initexit_lock || !is_self_couldbelinking()"
gofz=$1 tree=$2 runs=${3:-40}
for i in $(seq "$runs"); do
    podman run --rm --cpuset-cpus=6 --memory=2g -v "$gofz":/w/gofz:ro -v "$tree":/seedbox/progress:ro \
        --entrypoint /seedbox/progress/bin64/drrun docker.io/library/eclipse-temurin:21-jre-jammy \
        -opt_speed -disable_traces -ldstex2cas -no_hw_cache_consistency -fake_ctr_dic -- \
        /usr/bin/timeout -s KILL 600 /w/gofz exec $((i % 2 + 1)) 1 2>&1 | grep -E '^(OK|FAIL)' || echo "run $i: no result"
done
