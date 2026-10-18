import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.LinkedTransferQueue;
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.SynchronousQueue;

/** Producer/consumer hand-off through every blocking queue implementation. */
public final class Queues {
    static long run(BlockingQueue<Long> q, int producers, int consumers, int per) throws InterruptedException {
        long[] sums = new long[consumers];
        Thread[] ts = new Thread[producers + consumers];
        int total = producers * per;
        int share = total / consumers;
        for (int p = 0; p < producers; p++) {
            final long base = p * 1_000_000L;
            ts[p] = new Thread(() -> {
                try { for (int i = 0; i < per; i++) q.put(base + i); } catch (InterruptedException e) { throw new RuntimeException(e); }
            });
        }
        for (int c = 0; c < consumers; c++) {
            final int id = c;
            final int take = (c == consumers - 1) ? total - share * (consumers - 1) : share;
            ts[producers + c] = new Thread(() -> {
                try { for (int i = 0; i < take; i++) sums[id] += q.take(); } catch (InterruptedException e) { throw new RuntimeException(e); }
            });
        }
        for (Thread t : ts) t.start();
        Fz.join(ts);
        long s = 0;
        for (long x : sums) s += x;
        return s;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Queues", args);
        int per = 20_000 * Fz.scale;
        int p = 1 + (int) (Fz.seed % 3), c = 1 + (int) ((Fz.seed / 3) % 3);
        Fz.out("abq", run(new ArrayBlockingQueue<>(16), p, c, per));
        Fz.out("lbq", run(new LinkedBlockingQueue<>(), p, c, per));
        Fz.out("lbd", run(new LinkedBlockingDeque<>(64), p, c, per));
        Fz.out("sync", run(new SynchronousQueue<>(), p, c, per / 10));
        Fz.out("ltq", run(new LinkedTransferQueue<>(), p, c, per));
        Fz.out("pbq", run(new PriorityBlockingQueue<>(), p, c, per));
        ConcurrentLinkedQueue<Integer> clq = new ConcurrentLinkedQueue<>();
        Thread[] ts = new Thread[4];
        for (int t = 0; t < 4; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < per; i++) {
                    clq.add(i);
                    if ((i & 1) == 1) clq.poll();
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        Fz.out("clq", clq.size());
        Fz.done();
    }
}
