import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;

/** Virtual threads: mounting, parking, sleeping, pinning in synchronized blocks and continuation stack copies. */
public final class VirtualThreads {
    static long deep(int d, long acc) {
        if (d == 0) {
            Thread.yield();
            return acc;
        }
        return deep(d - 1, acc * 31 + d);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("VirtualThreads", args);
        int n = 2_000 * Fz.scale;
        AtomicLong total = new AtomicLong();
        ReentrantLock lock = new ReentrantLock();
        Object mon = new Object();
        long[] guarded = {0};
        try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
            List<Future<Long>> fs = new ArrayList<>();
            for (int i = 0; i < n; i++) {
                final int k = i;
                fs.add(ex.submit(() -> {
                    long r = deep(20 + k % 40, k);
                    if (k % 3 == 0) Thread.sleep(1 + k % 3);
                    lock.lock();
                    try { guarded[0]++; } finally { lock.unlock(); }
                    if (k % 50 == 0) synchronized (mon) { Thread.sleep(1); guarded[0] += 0; }
                    total.addAndGet(r & 0xffff);
                    return r;
                }));
            }
            long h = 0;
            for (Future<Long> f : fs) h = h * 31 + f.get();
            Fz.outHex("results", h);
        }
        Fz.out("total", total.get());
        Fz.out("guarded", guarded[0]);
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>(4);
        Thread prod = Thread.ofVirtual().start(() -> {
            try { for (int i = 0; i < 10_000; i++) q.put(i); q.put(-1); } catch (InterruptedException e) { throw new RuntimeException(e); }
        });
        long sum = 0;
        Thread cons = Thread.ofVirtual().unstarted(() -> {});
        int v;
        while ((v = q.take()) >= 0) sum += v;
        prod.join();
        Fz.out("pingpong", sum);
        Fz.out("isVirtual", Thread.ofVirtual().unstarted(() -> {}).isVirtual() + " " + cons.isVirtual());
        Fz.done();
    }
}
