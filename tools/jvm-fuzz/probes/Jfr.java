import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * Flight recorder with execution sampling (asynchronous thread suspension) over a busy workload.
 * Optional arguments after seed and scale: a comma-separated event set (exec, native, threaddump,
 * exceptions, alloc, monitor, park, all or none), the sampling period in milliseconds, and the
 * workload the samples land in: lcg, soe (stack overflow), npe (implicit null checks), alloc,
 * lock, vthread or native (zlib and CRC32).
 */
public final class Jfr {
    static volatile long sink;
    static volatile Object maybeNull;

    static int depth(int d) { return d + 1 + depth(d + 1); }

    /** One unit of the chosen workload; results do not depend on sampling. */
    static long work(String kind, long x, int i, Object lock) throws Exception {
        switch (kind) {
            case "soe" -> {
                if ((i & 1023) == 0) {
                    try { depth(0); } catch (StackOverflowError e) { x += 1; }
                }
            }
            case "npe" -> {
                Object o = (i & 7) == 0 ? null : lock;
                try { x += o.hashCode() == 0 ? 1 : 2; } catch (NullPointerException e) { x += 3; }
            }
            case "alloc" -> { long[] a = new long[1 + (i & 63)]; a[0] = x; x += a.length; }
            case "lock" -> { synchronized (lock) { x = x * 31 + i; } }
            case "native" -> {
                if ((i & 1023) == 0) {
                    java.util.zip.CRC32 crc = new java.util.zip.CRC32();
                    crc.update(new byte[8192]);
                    x += crc.getValue();
                }
            }
            default -> {}
        }
        return x * 6364136223846793005L + 1442695040888963407L;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Jfr", args);
        String events = args.length > 2 ? args[2] : "all";
        int period = args.length > 3 ? Integer.parseInt(args[3]) : 1;
        String workload = args.length > 4 ? args[4] : "lcg";
        boolean all = events.equals("all");
        Path file = Files.createTempFile("fz", ".jfr");
        long h = 0;
        try (Recording r = new Recording()) {
            if (all || events.contains("exec")) r.enable("jdk.ExecutionSample").withPeriod(Duration.ofMillis(period));
            if (all || events.contains("alloc")) r.enable("jdk.ObjectAllocationSample");
            if (all || events.contains("monitor")) r.enable("jdk.JavaMonitorEnter").withThreshold(Duration.ZERO);
            if (all || events.contains("park")) r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO);
            if (all || events.contains("native")) r.enable("jdk.NativeMethodSample").withPeriod(Duration.ofMillis(period));
            if (all || events.contains("threaddump")) r.enable("jdk.ThreadDump").withPeriod(Duration.ofMillis(10 * period));
            if (all || events.contains("exceptions")) r.enable("jdk.JavaExceptionThrow");
            if (!events.equals("none")) r.start();
            Thread[] ts = new Thread[3];
            long[] results = new long[ts.length];
            Object lock = new Object();
            for (int t = 0; t < ts.length; t++) {
                final int id = t;
                Runnable body = () -> {
                    long x = id;
                    try {
                        for (int i = 0; i < 2_000_000 * Fz.scale; i++) {
                            x = work(workload, x, i, lock);
                            if ((i & 4095) == 0) synchronized (lock) { sink += x; }
                            if ((i & 65535) == 0) java.util.concurrent.locks.LockSupport.parkNanos(100_000);
                        }
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                    synchronized (lock) { results[id] = x; }
                };
                ts[t] = workload.equals("vthread") ? Thread.ofVirtual().unstarted(body) : new Thread(body);
                ts[t].start();
            }
            Fz.join(ts);
            h = java.util.Arrays.hashCode(results);
            if (!events.equals("none")) {
                r.stop();
                r.dump(file);
            }
        }
        long samples = 0, total = 0;
        for (RecordedEvent e : events.equals("none") ? java.util.List.<RecordedEvent>of() : RecordingFile.readAllEvents(file)) {
            total++;
            if (e.getEventType().getName().equals("jdk.ExecutionSample")) samples++;
        }
        Files.delete(file);
        Fz.outHex("workload", h);
        // Event counts depend on sampling timing; only that execution sampling ran is checked.
        if (!workload.equals("vthread") && (all || events.contains("exec")))
            Fz.out("sampled", samples > 0);
        Fz.done();
    }
}
