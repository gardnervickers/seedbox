import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.nio.channels.Selector;
import java.util.concurrent.locks.LockSupport;

/**
 * Thread and process CPU-time clocks advance with work, and minimal timeouts return promptly.
 * Only invariants are printed.
 */
public final class Clocks {
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Fz.init("Clocks", args);
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long cpu0 = mx.getCurrentThreadCpuTime(), user0 = mx.getCurrentThreadUserTime();
        long wall0 = System.nanoTime();
        long h = 1;
        while (System.nanoTime() - wall0 < 300_000_000L) for (int i = 0; i < 100_000; i++) h = h * 31 + i;
        sink = h;
        long cpu1 = mx.getCurrentThreadCpuTime(), user1 = mx.getCurrentThreadUserTime();
        long wall1 = System.nanoTime();
        Fz.out("threadCpuAdvanced", cpu1 - cpu0 > 50_000_000L);
        Fz.out("threadCpuNotAboveWall", cpu1 - cpu0 <= (wall1 - wall0) + 20_000_000L);
        Fz.out("userTimeAdvanced", user1 >= user0);
        com.sun.management.OperatingSystemMXBean os =
            (com.sun.management.OperatingSystemMXBean) ManagementFactory.getOperatingSystemMXBean();
        Fz.out("processCpuPositive", os.getProcessCpuTime() > 0);
        long t0 = System.nanoTime();
        for (int i = 0; i < 200; i++) Thread.sleep(0, 1);
        for (int i = 0; i < 200; i++) LockSupport.parkNanos(1);
        try (Selector s = Selector.open()) { for (int i = 0; i < 100; i++) s.select(1); }
        Fz.out("shortWaitsReturn", System.nanoTime() - t0 < 30_000_000_000L);
        Fz.done();
    }
}
