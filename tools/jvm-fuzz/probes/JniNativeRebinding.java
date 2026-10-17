import java.lang.invoke.MethodHandles;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.LongUnaryOperator;

/**
 * Native methods rebound without redefinition: the JNI helper (libfzjni.so) binds T.f and T.g
 * with RegisterNatives to implementation A, compiled callers warm up (HotSpot generates a native
 * wrapper with the function address built in), then RegisterNatives binds B and the very next
 * call must return B's result; UnregisterNatives must make the next call throw
 * UnsatisfiedLinkError (T has no Java_ symbols to find); A is then restored while another thread
 * keeps compiling and dropping hidden classes. Each rebinding makes the old wrapper not entrant,
 * so callers must reach the newly patched entry. A second thread keeps calling T.g throughout
 * and accepts only A's result, B's result or UnsatisfiedLinkError.
 */
public final class JniNativeRebinding {
    /** 0 binds A, 1 binds B, 2 unregisters, 3 binds f to A and g to B. */
    static native int rebind(Class<?> c, int which);

    static final class T {
        static native long f(long x, int y, double d);
        static native long g(long x);
    }

    static long fA(long x, int y, double d) { return x * 31 + y ^ (Double.doubleToRawLongBits(d) >>> 3); }
    static long fB(long x, int y, double d) { return (x ^ 0x5bd1e995L) * 17 - y + (Double.doubleToRawLongBits(d) << 1); }
    static long gA(long x) { return Long.rotateLeft(x, 13) + 0x9E3779B97F4A7C15L; }
    static long gB(long x) { return Long.rotateRight(x, 7) ^ 0xC2B2AE3D27D4EB4FL; }

    // Compiled callers: the call sites C1 and C2 patch to the native wrapper.
    static long callF(long x, int y, double d) { return T.f(x, y, d); }
    static long callG(long x) { return T.g(x); }

    /** n calls of f and g, each checked against the bound implementation's model. */
    static long warm(int n, long seed, boolean fIsB, boolean gIsB, long[] bad) {
        long x = seed, acc = 0;
        for (int i = 0; i < n; i++) {
            x = x * 6364136223846793005L + 1442695040888963407L;
            int y = (int) (x >>> 40);
            double d = (double) (x >> 11) * 0x1p-30;
            long r = callF(x, y, d);
            if (r != (fIsB ? fB(x, y, d) : fA(x, y, d))) bad[0]++;
            long s = callG(x);
            if (s != (gIsB ? gB(x) : gA(x))) bad[0]++;
            acc = acc * 31 + r + s;
        }
        return acc;
    }

    /** Calls that must all throw UnsatisfiedLinkError; returns how many did. */
    static int unlinked(int n, long seed) {
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            try {
                if ((i & 1) == 0) callF(seed + i, i, i * 0.5);
                else callG(seed + i);
            } catch (UnsatisfiedLinkError e) {
                thrown++;
            }
        }
        return thrown;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("JniNativeRebinding", args);
        System.load(System.getProperty("fz.jnilib", "/fuzz/bin/libfzjni.so"));
        Fz.expect(rebind(T.class, 0) == 0, "RegisterNatives A");

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong bgBad = new AtomicLong();
        // Unrelated compilation while bindings change: hidden classes defined, compiled, dropped.
        Thread churn = new Thread(() -> {
            try (java.io.InputStream in = Fz.class.getResourceAsStream("/Payload.class")) {
                byte[] bytes = in.readAllBytes();
                long s = 0;
                while (!stop.get()) {
                    Class<?> c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                    var op = (LongUnaryOperator) c.getDeclaredConstructor().newInstance();
                    for (int i = 0; i < 20_000; i++) s += op.applyAsLong(i);
                    Thread.sleep(2);
                }
                if (s == 42) System.out.print("");
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        }, "fz-churn");
        churn.setDaemon(true);
        churn.start();
        // A concurrent caller sees any of the bindings, never anything else.
        Thread bg = new Thread(() -> {
            long x = Fz.seed;
            while (!stop.get()) {
                x = x * 6364136223846793005L + 1;
                try {
                    long r = callG(x);
                    if (r != gA(x) && r != gB(x)) bgBad.incrementAndGet();
                } catch (UnsatisfiedLinkError e) {
                    // unregistered at the moment
                }
            }
        }, "fz-bg");
        bg.start();

        long[] bad = {0};
        long acc = warm(40_000, Fz.seed, false, false, bad);
        int cycles = 30 * Fz.scale, thrownTotal = 0, expectedThrown = 0;
        for (int k = 0; k < cycles; k++) {
            long s = Fz.seed * 1_000_003L + k;
            int n = 2_000 + (int) (s & 1023);
            // B, checked from the very first call after RegisterNatives returns.
            Fz.expect(rebind(T.class, 1) == 0, "RegisterNatives B");
            acc = acc * 31 + warm(n, s, true, true, bad);
            // Unregistered: every call links again by name and finds nothing.
            Fz.expect(rebind(T.class, 2) == 0, "UnregisterNatives");
            int m = 50 + k;
            thrownTotal += unlinked(m, s);
            expectedThrown += m;
            // Mixed bindings, then A again.
            Fz.expect(rebind(T.class, 3) == 0, "RegisterNatives A/B");
            acc = acc * 31 + warm(n / 2, s ^ 0x55, false, true, bad);
            Fz.expect(rebind(T.class, 0) == 0, "RegisterNatives A");
            acc = acc * 31 + warm(n, s + 7, false, false, bad);
        }
        stop.set(true);
        bg.join();
        churn.join();
        Fz.out("cycles", cycles);
        Fz.out("badResults", bad[0]);
        Fz.out("unsatisfied", thrownTotal + " of " + expectedThrown);
        Fz.out("concurrentBad", bgBad.get());
        Fz.outHex("result", acc);
        Fz.mix(acc);
        Fz.mix(thrownTotal);
        Fz.expect(bad[0] == 0 && thrownTotal == expectedThrown && bgBad.get() == 0, "bindings");
        Fz.done();
    }
}
