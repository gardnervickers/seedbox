import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Job control against safepoint and handshake protocols: the probe starts itself again as a child
 * JVM (same flags, attach listener started at boot) that runs hot workers while its main thread
 * storms VM operations: safepoints (GC, all-thread dumps), per-thread handshakes (stack traces of
 * one thread) and thread CPU-time queries. The parent stops and continues the child at about
 * 20 Hz (SIGSTOP or SIGTSTP, then SIGCONT) until the child has finished its fixed work, and every
 * fourth stop sends `jcmd Thread.print` through the child's attach socket while it is stopped,
 * reading the reply after SIGCONT. The child's worker results and VM-operation count must match a
 * run without job control; every attach reply must list the workers.
 */
public final class JvmSigstopHandshakes {
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

    static long spin(long seed, int n) {
        long h = seed;
        for (int i = 0; i < n; i++) { h ^= h << 13; h ^= h >>> 7; h ^= h << 17; }
        return h;
    }

    static final String STARTED = "@@workers-started", PHASE_DONE = "@@phase-done";

    public static void main(String[] args) throws Exception {
        if (args.length > 2 && args[2].equals("child")) {
            child(args);
            return;
        }
        Fz.init("JvmSigstopHandshakes", args);
        List<String> cmd = new ArrayList<>();
        cmd.add(Path.of(System.getProperty("java.home"), "bin", "java").toString());
        cmd.addAll(ManagementFactory.getRuntimeMXBean().getInputArguments());
        cmd.add("-XX:+StartAttachListener");
        cmd.addAll(List.of("-cp", System.getProperty("java.class.path"), "JvmSigstopHandshakes",
                           Long.toString(Fz.seed), Integer.toString(Fz.scale), "child"));
        Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
        long pid = p.pid();
        List<String> lines = new ArrayList<>();
        AtomicBoolean started = new AtomicBoolean(), phaseDone = new AtomicBoolean();
        Thread reader = new Thread(() -> {
            try (BufferedReader r = new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
                for (String line; (line = r.readLine()) != null; ) {
                    if (line.equals(STARTED)) started.set(true);
                    else if (line.equals(PHASE_DONE)) phaseDone.set(true);
                    else synchronized (lines) { lines.add(line); }
                }
            } catch (java.io.IOException e) {
                throw new java.io.UncheckedIOException(e);
            }
        }, "fz-reader");
        reader.start();

        Path socket = Path.of("/tmp/.java_pid" + pid);
        int minCycles = 40, cycles = 0, attaches = 0, attachBad = 0, stopFailed = 0;
        while ((cycles < minCycles || !phaseDone.get()) && p.isAlive()) {
            Thread.sleep(30);
            int stopSig = cycles % 3 == 2 ? SIGTSTP : SIGSTOP;
            if (!kill(pid, stopSig)) stopFailed++;
            SocketChannel ch = null;
            // Thread dumps only once the workers run, so each reply must list them.
            if (cycles % 4 == 3 && started.get() && Files.exists(socket)) {
                // Connect and send the request while the child is stopped; the reply comes after SIGCONT.
                ch = SocketChannel.open(StandardProtocolFamily.UNIX);
                ch.connect(UnixDomainSocketAddress.of(socket));
                ByteArrayOutputStream req = new ByteArrayOutputStream();
                for (String part : new String[] {"1", "jcmd", "Thread.print", "", ""}) {
                    req.write(part.getBytes(StandardCharsets.UTF_8));
                    req.write(0);
                }
                ch.write(ByteBuffer.wrap(req.toByteArray()));
            }
            Thread.sleep(20);
            if (!kill(pid, SIGCONT)) stopFailed++;
            if (ch != null) {
                ByteArrayOutputStream resp = new ByteArrayOutputStream();
                ByteBuffer buf = ByteBuffer.allocate(65536);
                while (ch.read(buf) >= 0) {
                    resp.write(buf.array(), 0, buf.position());
                    buf.clear();
                }
                ch.close();
                String r = resp.toString(StandardCharsets.UTF_8);
                attaches++;
                if (!r.startsWith("0\n") || !r.contains("\"hs-worker-0\"")) attachBad++;
            }
            cycles++;
        }
        // Let the child finish: it runs extra VM operations until its stdin closes.
        try (OutputStream in = p.getOutputStream()) {
            in.write('x');
        } catch (java.io.IOException e) {
            // already gone
        }
        int code = p.waitFor();
        reader.join();
        synchronized (lines) {
            for (String line : lines) System.out.println(line);
        }
        Fz.out("childExit", code);
        Fz.out("stopCycles", cycles >= minCycles);
        Fz.out("attachReplies", attaches > 0 && attachBad == 0);
        Fz.out("stopFailures", stopFailed);
        Fz.mix(code);
        Fz.mix(lines.size());
        for (String line : lines) Fz.mix(line);
        Fz.expect(code == 0 && attaches > 0 && attachBad == 0 && stopFailed == 0, "child under job control");
        Fz.done();
    }

    static void child(String[] args) throws Exception {
        Fz.init("JvmSigstopHandshakes.child", args);
        int workers = 3 + (int) (Fz.seed % 2);
        int work = 2_000 * Fz.scale;
        long[] results = new long[workers];
        AtomicBoolean done = new AtomicBoolean();
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long h = id + 1;
                for (int i = 0; i < work; i++) {
                    h = spin(h, 10_000);
                    if ((i & 7) == id) { Object[] junk = new Object[64 + id]; h += junk.length; }
                    if (i % 97 == id) synchronized (results) { h ^= results.length; }
                }
                results[id] = h;
                long x = h;
                while (!done.get()) x = spin(x, 1_000);
            }, "hs-worker-" + w);
            ts[w].start();
        }
        System.out.println(STARTED);
        System.out.flush();
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        int ops = 150 * Fz.scale;
        long frames = 0;
        for (int op = 0; op < ops; op++) {
            switch (op % 6) {
                case 0 -> Thread.getAllStackTraces();
                case 1, 4 -> { for (Thread t : ts) frames += t.getStackTrace().length > 0 ? 1 : 0; }
                case 2 -> { ThreadInfo[] info = mx.dumpAllThreads(true, true); Fz.expect(info.length > workers, "thread dump"); }
                case 3 -> System.gc();
                default -> mx.getThreadCpuTime(ts[op % workers].getId());
            }
        }
        // The fixed work is over; keep the VM operations going until the parent lets us finish.
        System.out.println(PHASE_DONE);
        System.out.flush();
        Thread extra = new Thread(() -> {
            for (int op = 0; !done.get(); op++) {
                if (op % 2 == 0) for (Thread t : ts) t.getStackTrace();
                else System.gc();
                try { Thread.sleep(5); } catch (InterruptedException e) { return; }
            }
        }, "hs-extra");
        extra.setDaemon(true);
        extra.start();
        System.in.read();
        done.set(true);
        Fz.join(ts);
        extra.join();
        for (int w = 0; w < workers; w++) Fz.outHex("worker" + w, results[w]);
        Fz.out("vmOps", ops);
        Fz.out("handshakeFrames", frames);
        for (long r : results) Fz.mix(r);
        Fz.done();
    }
}
