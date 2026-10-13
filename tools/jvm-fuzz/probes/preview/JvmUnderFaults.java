import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/**
 * Three translator-heavy probes as concurrent child JVMs (SignalBomb: thread-directed signals into
 * compiled code; VthreadStorm: continuations over four carriers; DeoptStorm with unlimited traps:
 * implicit null checks, uncommon traps and recompilation) while the parent pauses and resumes them
 * as an operator would: every 250 ms of running, one child or all of them are stopped for 50 ms
 * (SIGSTOP, or SIGTSTP every third pause) and continued. Each child must print exactly what it
 * prints undisturbed and end with its OK line. Clock steps and CPU throttling are Seedbox oracle
 * faults (`clock_skew`, `throttle`); they need the harness scenario and are not done here.
 */
public final class JvmUnderFaults {
    static final int SIGCONT = 18, SIGSTOP = 19, SIGTSTP = 20;
    static final MethodHandle KILL;
    static {
        Linker linker = Linker.nativeLinker();
        KILL = linker.downcallHandle(linker.defaultLookup().find("kill").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
    }

    static boolean kill(long pid, int sig) {
        try {
            return (int) KILL.invokeExact((int) pid, sig) == 0;
        } catch (Throwable t) {
            throw new RuntimeException(t);
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("JvmUnderFaults", args);
        String java = Path.of(System.getProperty("java.home"), "bin", "java").toString();
        List<String> base = new ArrayList<>(ManagementFactory.getRuntimeMXBean().getInputArguments());
        // Each child gets its own error file and the flags its PROBES entry or profile uses.
        base.removeIf(a -> a.startsWith("-XX:ErrorFile="));
        String[][] children = {
            {"SignalBomb"},
            {"VthreadStorm", "-Djdk.virtualThreadScheduler.parallelism=4"},
            {"DeoptStorm", "-XX:PerBytecodeTrapLimit=1000000", "-XX:PerMethodTrapLimit=1000000",
             "-XX:PerMethodRecompilationCutoff=-1", "-XX:-OmitStackTraceInFastThrow"},
        };
        int n = children.length;
        Process[] ps = new Process[n];
        ByteArrayOutputStream[] outs = new ByteArrayOutputStream[n];
        Thread[] readers = new Thread[n];
        for (int c = 0; c < n; c++) {
            List<String> cmd = new ArrayList<>();
            cmd.add(java);
            cmd.addAll(base);
            for (int k = 1; k < children[c].length; k++) cmd.add(children[c][k]);
            cmd.add("-XX:ErrorFile=" + System.getProperty("java.io.tmpdir") + "/hs_err_child" + c + ".log");
            cmd.addAll(List.of("-cp", System.getProperty("java.class.path"), children[c][0],
                               Long.toString(Fz.seed + c), Integer.toString(Fz.scale)));
            ps[c] = new ProcessBuilder(cmd).redirectErrorStream(true).start();
            outs[c] = new ByteArrayOutputStream();
            final int k = c;
            readers[c] = new Thread(() -> {
                try (InputStream in = ps[k].getInputStream()) {
                    in.transferTo(outs[k]);
                } catch (java.io.IOException e) {
                    throw new java.io.UncheckedIOException(e);
                }
            }, "fz-reader-" + c);
            readers[c].start();
        }
        int pauses = 0, failed = 0;
        java.util.SplittableRandom pick = new java.util.SplittableRandom(Fz.seed);
        while (true) {
            boolean any = false;
            for (Process p : ps) any |= p.isAlive();
            if (!any) break;
            Thread.sleep(250);
            // One child (seeded choice) or, every fourth pause, all of them.
            int who = pauses % 4 == 3 ? -1 : pick.nextInt(n);
            int sig = pauses % 3 == 2 ? SIGTSTP : SIGSTOP;
            List<Process> paused = new ArrayList<>();
            for (int c = 0; c < n; c++) {
                if ((who < 0 || who == c) && ps[c].isAlive()) {
                    if (!kill(ps[c].pid(), sig) && ps[c].isAlive()) failed++;
                    paused.add(ps[c]);
                }
            }
            Thread.sleep(50);
            for (Process p : paused) {
                if (!kill(p.pid(), SIGCONT) && p.isAlive()) failed++;
            }
            pauses++;
        }
        boolean allOk = true;
        for (int c = 0; c < n; c++) {
            int code = ps[c].waitFor();
            readers[c].join();
            String name = children[c][0];
            Fz.out("child " + name + " exit", code);
            String text = outs[c].toString(StandardCharsets.UTF_8);
            for (String line : text.split("\n")) {
                if (line.isEmpty()) continue;
                Fz.out(name + "|", line);
                Fz.mix(line);
            }
            allOk &= code == 0 && text.contains("OK " + name + " ");
        }
        Fz.out("pauses", pauses > 0);
        Fz.out("signalFailures", failed);
        Fz.expect(allOk && failed == 0, "children under pause/resume");
        Fz.done();
    }
}
