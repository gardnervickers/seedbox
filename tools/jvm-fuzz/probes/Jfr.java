import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * Flight recorder with execution sampling (asynchronous thread suspension) over a busy workload.
 * Optional arguments after seed and scale: a comma-separated event set (exec, alloc, monitor, park,
 * all or none) and the execution sampling period in milliseconds.
 */
public final class Jfr {
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Fz.init("Jfr", args);
        String events = args.length > 2 ? args[2] : "all";
        int period = args.length > 3 ? Integer.parseInt(args[3]) : 1;
        boolean all = events.equals("all");
        Path file = Files.createTempFile("fz", ".jfr");
        long h = 0;
        try (Recording r = new Recording()) {
            if (all || events.contains("exec")) r.enable("jdk.ExecutionSample").withPeriod(Duration.ofMillis(period));
            if (all || events.contains("alloc")) r.enable("jdk.ObjectAllocationSample");
            if (all || events.contains("monitor")) r.enable("jdk.JavaMonitorEnter").withThreshold(Duration.ZERO);
            if (all || events.contains("park")) r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO);
            if (!events.equals("none")) r.start();
            Thread[] ts = new Thread[3];
            long[] results = new long[ts.length];
            Object lock = new Object();
            for (int t = 0; t < ts.length; t++) {
                final int id = t;
                ts[t] = new Thread(() -> {
                    long x = id;
                    for (int i = 0; i < 2_000_000 * Fz.scale; i++) {
                        x = x * 6364136223846793005L + 1442695040888963407L;
                        if ((i & 4095) == 0) synchronized (lock) { sink += x; }
                        if ((i & 65535) == 0) java.util.concurrent.locks.LockSupport.parkNanos(100_000);
                    }
                    synchronized (lock) { results[id] = x; }
                });
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
        // Event counts depend on sampling timing; reading the file back is the check.
        Fz.mix(0);
        Fz.done();
    }
}
