import java.util.Arrays;
import java.util.SplittableRandom;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * JNI critical regions under collection: worker threads hold GetPrimitiveArrayCritical (one,
 * two or three arrays nested) during bounded native work from the helper library libfzjni.so,
 * read under a critical region released with JNI_ABORT, read strings with GetStringCritical and
 * use the copying Get/Release<Int>ArrayElements, while another thread allocates and requests
 * collections. Each worker keeps references live in compiled code across the native calls and
 * checks them afterwards; every array element is checked against a Java model. Pinning and
 * copying give the same results.
 */
public final class JniCriticalGc {
    static native long critical(int[] a, long[] b, double[] c, int passes, long salt, int nested);
    static native long readAbort(int[] a);
    static native long stringCritical(String s);
    static native long elements(int[] a, int passes, long salt);

    static final class Holder {
        final int[] a;
        final long[] b;
        final double[] c;
        final long tag;
        Holder next;

        Holder(int[] a, long[] b, double[] c, long tag) { this.a = a; this.b = b; this.c = c; this.tag = tag; }
    }

    /** The native work on copies: the reference for every element and the returned hash. */
    static long model(int[] a, long[] b, double[] c, int passes, long salt) {
        int nb = b == null ? 1 : b.length, nc = c == null ? 1 : c.length;
        for (int p = 0; p < passes; p++) {
            for (int i = 0; i < a.length; i++) {
                a[i] = a[i] * 0x01000193 ^ (i + (int) salt + p);
                if (b != null) b[i % nb] += a[i];
                if (c != null) c[i % nc] = c[i % nc] * 0.5 + a[i];
            }
        }
        long h = 0xcbf29ce484222325L;
        for (int x : a) h = (h ^ (x & 0xffffffffL)) * 0x100000001B3L;
        if (b != null) for (long x : b) h = (h ^ x) * 0x100000001B3L;
        if (c != null) for (double x : c) h = (h ^ Double.doubleToRawLongBits(x)) * 0x100000001B3L;
        return h;
    }

    static volatile Object sink;

    /** One worker's seeded sequence of critical sections; returns a hash of every result. */
    static long work(int id, int iters, long[] bad) {
        SplittableRandom r = new SplittableRandom(Fz.seed * 31 + id);
        Holder chain = null;
        long acc = 0;
        for (int it = 0; it < iters; it++) {
            int n = 256 + r.nextInt(1792), passes = 1 + r.nextInt(3), nested = r.nextInt(3);
            long salt = r.nextLong();
            int[] a = new int[n];
            for (int i = 0; i < n; i++) a[i] = r.nextInt();
            long[] b = new long[1 + r.nextInt(64)];
            double[] c = new double[1 + r.nextInt(64)];
            for (int i = 0; i < c.length; i++) c[i] = r.nextInt(1 << 20) * 0.25;
            Holder h = new Holder(a, b, c, salt ^ it);
            h.next = chain;
            chain = (it % 16 == 0) ? null : h;  // a short chain of live holders across the calls
            int[] ea = a.clone();
            long[] eb = nested >= 1 ? b.clone() : null;
            double[] ec = nested >= 2 ? c.clone() : null;
            long want = model(ea, eb, ec, passes, salt);
            long got = critical(h.a, h.b, h.c, passes, salt, nested);
            boolean ok = got == want && Arrays.equals(h.a, ea) && h.tag == (salt ^ it)
                && (eb == null || Arrays.equals(h.b, eb)) && (ec == null || Arrays.equals(h.c, ec));
            for (Holder k = h.next; k != null; k = k.next) ok &= k.a.length >= 256 && k.b.length >= 1;
            if (!ok) bad[id]++;
            acc = acc * 31 + got;
            switch (it % 4) {
                case 0 -> {
                    long s = 0;
                    for (int x : h.a) s = s * 31 + (x & 0xffffffffL);
                    if (readAbort(h.a) != s) bad[id]++;
                }
                case 1 -> {
                    String str = "crit-" + Long.toHexString(salt) + (it % 3 == 0 ? "€" : "");
                    if (stringCritical(str) != str.hashCode()) bad[id]++;
                }
                case 2 -> {
                    int[] e = h.a.clone();
                    long w = model(e, null, null, 1, ~salt);
                    if (elements(h.a, 1, ~salt) != w || !Arrays.equals(h.a, e)) bad[id]++;
                }
                default -> sink = h;
            }
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("JniCriticalGc", args);
        System.load(System.getProperty("fz.jnilib", "/fuzz/bin/libfzjni.so"));
        AtomicBoolean stop = new AtomicBoolean();
        // A fixed allocation budget (not a duration), so a slower run does not collect more often.
        Thread pressure = new Thread(() -> {
            Object[] ring = new Object[256];
            SplittableRandom r = new SplittableRandom(Fz.seed);
            for (int i = 0; i < 40_000 * Fz.scale && !stop.get(); i++) {
                ring[i & 255] = new long[r.nextInt(1, 8192)];
                if (i % 4096 == 4095) System.gc();
            }
            sink = ring;
        }, "fz-pressure");
        pressure.setDaemon(true);
        pressure.start();
        int workers = 3, iters = 600 * Fz.scale;
        long[] res = new long[workers], bad = new long[workers];
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> res[id] = work(id, iters, bad), "fz-crit-" + w);
            ts[w].start();
        }
        Fz.join(ts);
        stop.set(true);
        pressure.join();
        for (int w = 0; w < workers; w++) {
            Fz.out("worker" + w, Long.toHexString(res[w]) + " bad " + bad[w]);
            Fz.mix(res[w]);
            Fz.mix(bad[w]);
        }
        Fz.done();
    }
}
