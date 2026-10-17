/**
 * Code dense in thread-register (x28 on AArch64) accesses: Thread.currentThread(), ThreadLocal
 * reads and writes, small allocations (TLAB bumps and slow paths) and counted loops with
 * safepoint polls, on two threads. Pair with the x28storm profile.
 */
public final class X28Storm {
    static final ThreadLocal<long[]> LOCAL = ThreadLocal.withInitial(() -> new long[4]);

    static long round(int n, long seed) {
        long h = seed;
        long[] cell = LOCAL.get();
        for (int i = 0; i < n; i++) {
            Thread t = Thread.currentThread();
            cell[i & 3] += i;
            if ((i & 63) == 0) LOCAL.set(cell = new long[4 + (i & 3)]);
            Object[] o = new Object[1 + (i & 7)];
            o[0] = t;
            h = h * 31 + o.length + cell[i & 3] + (t.isDaemon() ? 1 : 0);
        }
        return h;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("X28Storm", args);
        long[] results = new long[2];
        Thread[] ts = new Thread[2];
        for (int w = 0; w < 2; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long first = 0;
                for (int r = 0; r < 20 * Fz.scale; r++) {
                    long h = round(20_000, id + 1);
                    if (r == 0) first = h;
                    else Fz.expect(h == first, "round " + r + " differs");
                }
                results[id] = first;
            });
            ts[w].start();
        }
        Fz.join(ts);
        Fz.outHex("t0", results[0]);
        Fz.outHex("t1", results[1]);
        Fz.done();
    }
}
