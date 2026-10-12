import java.util.concurrent.locks.LockSupport;

/** Interrupting sleeping, waiting, joined and parked threads; park/unpark permits. */
public final class Interrupts {
    public static void main(String[] args) throws Exception {
        Fz.init("Interrupts", args);
        int rounds = 100 * Fz.scale;
        int[] seen = new int[4];
        Object mon = new Object();
        for (int r = 0; r < rounds; r++) {
            final int kind = r % 4;
            Thread t = new Thread(() -> {
                try {
                    switch (kind) {
                        case 0 -> Thread.sleep(60_000);
                        case 1 -> { synchronized (mon) { mon.wait(); } }
                        case 2 -> Thread.currentThread().join();
                        default -> {
                            while (!Thread.currentThread().isInterrupted()) LockSupport.park();
                            throw new InterruptedException();
                        }
                    }
                } catch (InterruptedException e) {
                    synchronized (seen) { seen[kind]++; }
                }
            });
            t.start();
            while (t.getState() == Thread.State.RUNNABLE || t.getState() == Thread.State.NEW) Thread.onSpinWait();
            t.interrupt();
            t.join();
        }
        Fz.out("interrupted", java.util.Arrays.toString(seen));
        int[] unparks = {0};
        Thread parker = new Thread(() -> {
            for (int i = 0; i < 1000; i++) {
                LockSupport.park();
                synchronized (unparks) { unparks[0]++; unparks.notify(); }
            }
        });
        parker.start();
        for (int i = 0; i < 1000; i++) {
            synchronized (unparks) {
                LockSupport.unpark(parker);
                while (unparks[0] <= i && parker.isAlive()) {
                    unparks.wait(1);
                    LockSupport.unpark(parker);
                }
            }
        }
        parker.join();
        Fz.out("unparks", unparks[0]);
        Thread.currentThread().interrupt();
        Fz.out("selfInterrupt", Thread.interrupted() + " " + Thread.interrupted());
        long t0 = System.nanoTime();
        LockSupport.parkNanos(5_000_000);
        Fz.out("parkNanosElapsed", System.nanoTime() - t0 >= 4_000_000 || Thread.interrupted());
        Fz.done();
    }
}
