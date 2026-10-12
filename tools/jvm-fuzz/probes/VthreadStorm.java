import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Many virtual threads over several carriers (run with a parallelism above one): deep stacks
 * at each park so continuations freeze and thaw many frames, timed sleeps, a contended lock
 * and pinning in synchronized blocks.
 */
public final class VthreadStorm {
    static long deep(int d, long h) throws InterruptedException {
        if (d == 0) {
            Thread.sleep(1);
            return h;
        }
        return deep(d - 1, h * 31 + d) + 1;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("VthreadStorm", args);
        AtomicLong total = new AtomicLong();
        ReentrantLock lock = new ReentrantLock();
        Object mon = new Object();
        long[] guarded = {0};
        int n = 5_000 * Fz.scale;
        try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < n; i++) {
                final int k = i;
                ex.submit(() -> {
                    long h = deep(50 + k % 100, k);
                    lock.lock();
                    try { guarded[0]++; } finally { lock.unlock(); }
                    if (k % 100 == 0) synchronized (mon) { Thread.sleep(1); }
                    total.addAndGet(h & 0xffff);
                    return null;
                });
            }
        }
        Fz.out("total", total.get());
        Fz.out("guarded", guarded[0]);
        Fz.done();
    }
}
