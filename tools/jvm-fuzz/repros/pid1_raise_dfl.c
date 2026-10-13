// A process that is PID 1 of its PID namespace raises a signal whose action is SIG_DFL, as
// coreutils `timeout` does when its child dumped core (signal(sig, SIG_DFL); unblock; raise).
// The kernel drops default-action signals that init sends itself, so natively raise() returns
// and the program goes on. Under the translator the process spins at 100% CPU forever, with
// the signal left pending (ShdPnd shows it) and most signals blocked.
//
// Seen first when a JVM probe crashed under `drrun ... -- /usr/bin/timeout ... java`: timeout
// was the container's PID 1, reported "the monitored command dumped core" and never exited.
//
// Build: gcc -O2 -o pid1_raise_dfl pid1_raise_dfl.c
// Native (PID 1 in a container): prints "still alive after raise(SIGSEGV)", exit 0.
//   podman run --rm -v $PWD:/w:ro eclipse-temurin:21-jre-jammy /w/pid1_raise_dfl
// Translator (the program is PID 1): never exits; `top` shows it running at ~100% CPU and
// /proc/PID/status has ShdPnd 0000000000024000, SigBlk fffffffffffbfab7.
//   podman run --rm -v $PWD:/w:ro -v DRTREE:/seedbox/progress:ro \
//     --entrypoint /seedbox/progress/bin64/drrun eclipse-temurin:21-jre-jammy \
//     -opt_speed -disable_traces -ldstex2cas -no_hw_cache_consistency -fake_ctr_dic -- /w/pid1_raise_dfl
// When it is not PID 1 (e.g. under `timeout`), it dies by SIGSEGV in both cases, as expected.
#include <signal.h>
#include <stdio.h>
#include <sys/resource.h>

int main(void) {
    struct rlimit rl = {0, 0};
    setrlimit(RLIMIT_CORE, &rl);
    sigset_t all;
    sigfillset(&all);
    sigprocmask(SIG_BLOCK, &all, NULL);
    signal(SIGSEGV, SIG_DFL);
    sigset_t one;
    sigemptyset(&one);
    sigaddset(&one, SIGSEGV);
    sigprocmask(SIG_UNBLOCK, &one, NULL);
    raise(SIGSEGV);
    puts("still alive after raise(SIGSEGV)");
    return 0;
}
