import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.concurrent.Semaphore;
import java.util.concurrent.atomic.AtomicLong;
import jdk.test.whitebox.WhiteBox;

/**
 * Deoptimization that must rebuild objects and locks C2 removed. DmHot.hot holds
 * two aliases of one non-escaping Box (read back through a Holder), an inner object and three
 * nested synchronized blocks on them, so C2 scalar-replaces the objects and eliminates the locks.
 * Each round defines a fresh hidden copy of DmHot (a fresh profile), warms it with its checking
 * branches never taken, compiles it at C2 (WhiteBox), and then deoptimizes it at a
 * controlled point: WhiteBox deoptimizeFrames or deoptimizeMethod from inside the call, a
 * deoptimizeFrames from another thread while the call waits, or a never-taken branch's
 * uncommon trap. The rebuilt frame must have one object behind both aliases, intact fields, the
 * monitors held the right number of times, and a contending thread must acquire the object only
 * after the frame releases it. -Dfz.diag=true (never set by the corpus) reports each round's
 * compilation level before and after the deoptimizing call on stderr.
 */
public final class DeoptMaterialization {
    static WhiteBox wb;
    static final boolean DIAG = Boolean.getBoolean("fz.diag");
    static Method hotMethod;
    static final AtomicLong failures = new AtomicLong();
    static final Semaphore deoptRequest = new Semaphore(0), deoptDone = new Semaphore(0);

    static final class Inner {
        long v;
        Inner(long v) { this.v = v; }
    }

    static final class Box {
        long a;
        int b;
        double d;
        Inner in;
        Box(long a, int b, double d, Inner in) { this.a = a; this.b = b; this.d = d; this.in = in; }
    }

    static final class Holder {
        Box ref;
        long tag;
    }

    static void fail(boolean ok) { if (!ok) failures.incrementAndGet(); }

    /** The controlled point, never inlined into hot. */
    static void point(int mode) {
        switch (mode) {
            case 1 -> wb.deoptimizeFrames(false);
            case 2 -> wb.deoptimizeMethod(hotMethod);
            case 3 -> {
                deoptRequest.release();
                deoptDone.acquireUninterruptibly();
            }
            default -> { }  // 0: warming; 4: the uncommon trap after the call deoptimizes
        }
    }

    static long model(long x, int mode) {
        long a = x + 3, inv = x ^ 0x5EED;
        int b = (int) (x >>> 32) ^ 5;
        double d = (double) (x & 0xffff) * 0.5;
        long tag = x * 3 + b;
        long r = a * 31 + b + (long) (d * 4) + inv + tag;
        if (mode != 0) r += 1 + 2 + 4;
        return r + inv + 1;
    }

    /** In the rebuilt frame, inside all three monitors. */
    static long inside(long x, Box p, Box q, Holder h) {
        fail(p == q && h.ref == p && p.in != null);
        fail(p.a == x + 3 && p.b == ((int) (x >>> 32) ^ 5) && p.d == (double) (x & 0xffff) * 0.5);
        fail(p.in.v == (x ^ 0x5EED) && h.tag == x * 3 + p.b);
        fail(Thread.holdsLock(p) && Thread.holdsLock(h));
        return 1;
    }

    static Thread contender;
    static long acquiredAt, releasedAt;
    static final AtomicLong order = new AtomicLong();

    /** Holding only p (twice entered, once left): another thread must block on it. */
    static long afterInner(Box p, Holder h) {
        fail(Thread.holdsLock(p) && !Thread.holdsLock(h));
        contender = new Thread(() -> {
            synchronized (p) { acquiredAt = order.incrementAndGet(); }
        }, "fz-contender");
        contender.start();
        boolean blocked = false;
        for (int i = 0; i < 60_000 && !(blocked = contender.getState() == Thread.State.BLOCKED); i++) {
            try { Thread.sleep(1); } catch (InterruptedException e) { throw new RuntimeException(e); }
        }
        fail(blocked);
        releasedAt = order.incrementAndGet();
        return 2;
    }

    static long afterAll(Box p) {
        fail(!Thread.holdsLock(p));
        try { contender.join(); } catch (InterruptedException e) { throw new RuntimeException(e); }
        fail(acquiredAt > releasedAt);
        synchronized (p) { fail(Thread.holdsLock(p)); }
        return 4;
    }

    /**
     * Compiles m at C2. A request made while a tier-3 compilation of m is queued is dropped, so
     * it is repeated until the level is reached (or a minute passes).
     */
    static boolean compileC2(WhiteBox wb, Method m) throws InterruptedException {
        for (int w = 0; w < 60_000 && wb.getMethodCompilationLevel(m) != 4; w++) {
            if (w % 10 == 0 && !wb.isMethodQueuedForCompilation(m)) wb.enqueueMethodForCompilation(m, 4);
            Thread.sleep(1);
        }
        return wb.getMethodCompilationLevel(m) == 4;
    }

    public static void main(String[] args) throws Throwable {
        Fz.init("DeoptMaterialization", args);
        wb = WhiteBox.getWhiteBox();
        byte[] template;
        try (java.io.InputStream in = DeoptMaterialization.class.getResourceAsStream("/DmHot.class")) {
            template = in.readAllBytes();
        }
        Method pointMethod = DeoptMaterialization.class.getDeclaredMethod("point", int.class);
        wb.testSetDontInlineMethod(pointMethod, true);
        boolean tiered = wb.getBooleanVMFlag("TieredCompilation");
        boolean c2 = wb.getBooleanVMFlag("UseCompiler") && (!tiered || wb.getIntxVMFlag("TieredStopAtLevel") >= 4);
        Thread deopter = new Thread(() -> {
            while (true) {
                deoptRequest.acquireUninterruptibly();
                wb.deoptimizeFrames(false);
                deoptDone.release();
            }
        }, "fz-deopter");
        deopter.setDaemon(true);
        deopter.start();
        int rounds = 24 * Fz.scale;
        long warmBad = 0, roundBad = 0, notC2 = 0, acc = 0;
        int[] modes = new int[5];
        for (int round = 0; round < rounds; round++) {
            MethodHandles.Lookup lookup = MethodHandles.lookup().defineHiddenClass(template, true);
            MethodHandle hot = lookup.findStatic(lookup.lookupClass(), "hot",
                                                 MethodType.methodType(long.class, long.class, int.class));
            hotMethod = lookup.lookupClass().getDeclaredMethod("hot", long.class, int.class);
            long base = Fz.rnd.nextLong();
            for (int i = 0; i < 20_000; i++) {
                long x = base + i * 0x9E3779B97F4A7C15L;
                if ((long) hot.invokeExact(x, 0) != model(x, 0)) warmBad++;
            }
            if (c2) {
                if (!compileC2(wb, hotMethod)) notC2++;
            }
            int mode = 1 + round % 4;
            modes[mode]++;
            long x = Fz.rnd.nextLong();
            int before = wb.getMethodCompilationLevel(hotMethod);
            long r = (long) hot.invokeExact(x, mode);
            if (DIAG) System.err.println("diag round " + round + " mode " + mode + " level " + before + " -> "
                                         + wb.getMethodCompilationLevel(hotMethod));
            if (r != model(x, mode)) roundBad++;
            acc = acc * 31 + r;
        }
        Fz.out("rounds", rounds + " modes " + modes[1] + "," + modes[2] + "," + modes[3] + "," + modes[4]);
        Fz.out("warmMismatches", warmBad);
        Fz.out("roundMismatches", roundBad);
        Fz.out("frameFailures", failures.get());
        Fz.out("notC2", notC2);
        Fz.outHex("result", acc);
        Fz.mix(acc);
        Fz.mix(warmBad + roundBad + failures.get() + notC2);
        Fz.done();
    }
}

/**
 * Defined again as a hidden class for every round of DeoptMaterialization, so each round's C2
 * code is built from a fresh profile in which the mode != 0 branches were never taken (their
 * calls are uncommon traps, so nothing escapes and C2 removes the allocations and locks).
 */
final class DmHot {
    static long hot(long x, int mode) {
        DeoptMaterialization.Inner in = new DeoptMaterialization.Inner(x ^ 0x5EED);
        DeoptMaterialization.Box p = new DeoptMaterialization.Box(x, (int) (x >>> 32), (double) (x & 0xffff) * 0.5, in);
        DeoptMaterialization.Holder h = new DeoptMaterialization.Holder();
        h.ref = p;
        h.tag = x * 3;
        DeoptMaterialization.Box q = h.ref;
        long r;
        synchronized (p) {
            p.a += 3;
            synchronized (q) {
                q.b ^= 5;
                synchronized (h) {
                    h.tag += p.b;
                    DeoptMaterialization.point(mode);
                    r = p.a * 31 + q.b + (long) (p.d * 4) + p.in.v + h.tag;
                    if (mode != 0) r += DeoptMaterialization.inside(x, p, q, h);
                }
                q.in.v += 1;
            }
            if (mode != 0) r += DeoptMaterialization.afterInner(p, h);
        }
        if (mode != 0) r += DeoptMaterialization.afterAll(p);
        return r + p.in.v;
    }
}
