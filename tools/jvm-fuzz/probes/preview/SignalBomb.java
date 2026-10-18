import java.util.Arrays;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import sun.misc.Signal;

/**
 * Asynchronous signals aimed at threads running hot compiled code: spin, allocation, monitor
 * hand-off, recursion and sorting. Each signal is thread-directed (tgkill) and paced on its
 * handler, so counts are deterministic; workers keep running until every signal is handled.
 * Requires --enable-preview (foreign-function API on JDK 21).
 */
public final class SignalBomb {
    static final AtomicBoolean done = new AtomicBoolean();
    static final Object lock = new Object();
    static volatile long sink;

    static long work(int kind, long seed) {
        long h = seed;
        switch (kind) {
            case 0 -> { for (int i = 0; i < 20_000; i++) h = h * 6364136223846793005L + 1442695040888963407L; }
            case 1 -> { for (int i = 0; i < 2_000; i++) { long[] a = new long[1 + (i & 15)]; a[0] = h; h += a.length; } }
            case 2 -> { for (int i = 0; i < 2_000; i++) synchronized (lock) { h = h * 31 + i; } }
            case 3 -> h = recurse(200, h);
            default -> { int[] a = new int[512]; for (int i = 0; i < a.length; i++) a[i] = (int) (h * (i + 1)); Arrays.sort(a); h += a[7]; }
        }
        return h;
    }

    static long recurse(int d, long h) { return d == 0 ? h : recurse(d - 1, h * 31 + d) + 1; }

    public static void main(String[] args) throws Exception {
        Fz.init("SignalBomb", args);
        Semaphore handled = new Semaphore(0);
        int[] counts = new int[2];
        Signal.handle(new Signal("HUP"), s -> { counts[0]++; handled.release(); });
        Signal.handle(new Signal("WINCH"), s -> { counts[1]++; handled.release(); });
        int workers = 5;
        long[] results = new long[workers];
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int kind = w;
            ts[w] = new Thread(() -> {
                long h = kind + 1;
                for (int r = 0; r < 200 * Fz.scale; r++) h = work(kind, h);
                results[kind] = h;
                long x = h;
                while (!done.get()) x = work(kind, x);
                sink = x;
            }, "sb-w" + w);
            ts[w].start();
        }
        int[] tids = new int[workers];
        for (int w = 0; w < workers; w++) tids[w] = Native.tid("sb-w" + w);
        int signals = 400 * Fz.scale, sent = 0;
        for (int i = 0; i < signals; i++) {
            if (Native.tgkill(tids[i % workers], (i & 1) == 0 ? Native.SIGHUP : Native.SIGWINCH)) sent++;
            Fz.expect(handled.tryAcquire(30, TimeUnit.SECONDS), "signal " + i + " not handled");
        }
        done.set(true);
        Fz.join(ts);
        Fz.out("signals", sent + " " + counts[0] + " " + counts[1]);
        for (int w = 0; w < workers; w++) Fz.outHex("worker" + w, results[w]);
        Fz.done();
    }
}
