import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.Exchanger;
import java.util.concurrent.Phaser;
import java.util.concurrent.Semaphore;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

/** java.util.concurrent synchronizers under contention. */
public final class JucLocks {
    public static void main(String[] args) throws Exception {
        Fz.init("JucLocks", args);
        int n = 3 + (int) (Fz.seed % 3);
        int per = 20_000 * Fz.scale;
        ReentrantLock fair = new ReentrantLock(true);
        ReentrantLock unfair = new ReentrantLock();
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        StampedLock sl = new StampedLock();
        Semaphore sem = new Semaphore(2);
        CyclicBarrier barrier = new CyclicBarrier(n);
        Phaser phaser = new Phaser(n);
        CountDownLatch start = new CountDownLatch(1);
        long[] c = new long[6];
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    for (int i = 0; i < per; i++) {
                        ReentrantLock l = (i & 1) == 0 ? fair : unfair;
                        l.lock();
                        try { c[(i & 1)]++; } finally { l.unlock(); }
                        if ((i & 7) == 0) {
                            rw.writeLock().lock();
                            try { c[2]++; } finally { rw.writeLock().unlock(); }
                        } else {
                            rw.readLock().lock();
                            try { Fz.expect(c[2] >= 0, "rw"); } finally { rw.readLock().unlock(); }
                        }
                        long st = sl.writeLock();
                        c[3]++;
                        sl.unlockWrite(st);
                        if ((i & 255) == 0) {
                            sem.acquire();
                            synchronized (c) { c[4]++; }
                            sem.release();
                        }
                        if ((i % (per / 4)) == 0) {
                            barrier.await();
                            phaser.arriveAndAwaitAdvance();
                        }
                    }
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
            ts[t].start();
        }
        start.countDown();
        Fz.join(ts);
        Fz.out("counts", java.util.Arrays.toString(c));
        ReentrantLock lock = new ReentrantLock();
        Condition cond = lock.newCondition();
        int[] box = {0};
        Thread waiter = new Thread(() -> {
            lock.lock();
            try {
                while (box[0] < 1000) {
                    box[0]++;
                    cond.signal();
                    cond.awaitUninterruptibly();
                }
                cond.signal();
            } finally {
                lock.unlock();
            }
        });
        lock.lock();
        waiter.start();
        try {
            while (box[0] < 1000) {
                cond.awaitUninterruptibly();
                if (box[0] < 1000) box[0]++;
                cond.signal();
            }
        } finally {
            lock.unlock();
        }
        waiter.join();
        Fz.out("condition", box[0]);
        Exchanger<Integer> ex = new Exchanger<>();
        long[] sum = {0};
        Thread other = new Thread(() -> {
            try { for (int i = 0; i < 500; i++) sum[0] += ex.exchange(i * 2); } catch (InterruptedException e) { throw new RuntimeException(e); }
        });
        other.start();
        long mine = 0;
        for (int i = 0; i < 500; i++) mine += ex.exchange(i * 3);
        other.join();
        Fz.out("exchange", mine + " " + sum[0]);
        Fz.done();
    }
}
