/** Contended synchronized blocks, recursive locking, wait/notify hand-off and monitor inflation. */
public final class Monitors {
    static final Object LOCK = new Object();
    static long counter;
    static int turn;

    static synchronized void bump(long[] box) { box[0]++; }

    static void recursive(Object o, int d) {
        synchronized (o) {
            counter++;
            if (d > 0) recursive(o, d - 1);
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Monitors", args);
        int threads = 2 + (int) (Fz.seed & 3);
        int per = 50_000 * Fz.scale;
        long[] box = new long[1];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < per; i++) {
                    synchronized (LOCK) { counter++; }
                    bump(box);
                    if ((i & 1023) == 0) {
                        Object o = new Object();
                        synchronized (o) { System.identityHashCode(o); }
                        recursive(LOCK, 5);
                    }
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        Fz.out("counter", counter);
        Fz.out("bump", box[0]);
        int rounds = 2_000 * Fz.scale;
        int players = 3;
        Thread[] ps = new Thread[players];
        long[] moves = new long[players];
        for (int p = 0; p < players; p++) {
            final int me = p;
            ps[p] = new Thread(() -> {
                for (int r = 0; r < rounds; r++) {
                    synchronized (LOCK) {
                        while (turn != me) {
                            try { LOCK.wait(); } catch (InterruptedException e) { throw new RuntimeException(e); }
                        }
                        moves[me]++;
                        turn = (turn + 1) % players;
                        LOCK.notifyAll();
                    }
                }
            });
            ps[p].start();
        }
        Fz.join(ps);
        Fz.out("moves", java.util.Arrays.toString(moves));
        Fz.done();
    }
}
