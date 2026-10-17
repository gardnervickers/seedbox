import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.AtomicBoolean;

/** Global and per-thread VM operations (GC, stack sampling, thread dumps) while other threads run hot loops. */
public final class Safepoints {
    static long spin(long seed, int n) {
        long h = seed;
        for (int i = 0; i < n; i++) { h ^= h << 13; h ^= h >>> 7; h ^= h << 17; }
        return h;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Safepoints", args);
        int workers = 2 + (int) (Fz.seed % 3);
        int work = 3_000 * Fz.scale;
        long[] results = new long[workers];
        AtomicBoolean done = new AtomicBoolean();
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long h = id + 1;
                for (int i = 0; i < work; i++) {
                    h = spin(h, 10_000);
                    if ((i & 7) == id) { Object[] junk = new Object[64]; h += junk.length; }
                }
                results[id] = h;
                // Keep running hot code until the VM operations are over, so every operation
                // meets compiled loops regardless of how fast the fixed work finished.
                long x = h;
                while (!done.get()) x = spin(x, 1_000);
            }, "worker-" + w);
            ts[w].start();
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        int ops = 60 * Fz.scale;
        for (int op = 0; op < ops; op++) {
            switch (op % 5) {
                case 0 -> Thread.getAllStackTraces();
                case 1 -> { for (Thread t : ts) t.getStackTrace(); }
                case 2 -> { ThreadInfo[] info = mx.dumpAllThreads(true, true); Fz.expect(info.length > workers, "thread dump"); }
                case 3 -> System.gc();
                default -> mx.getThreadCpuTime(ts[op % workers].getId());
            }
        }
        done.set(true);
        Fz.join(ts);
        for (int w = 0; w < workers; w++) Fz.outHex("worker" + w, results[w]);
        Fz.out("vmOps", ops);
        Fz.done();
    }
}
