/* **********************************************************
 * Copyright (c) 2026 Google, Inc.  All rights reserved.
 * **********************************************************/

/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * * Redistributions of source code must retain the above copyright notice,
 *   this list of conditions and the following disclaimer.
 *
 * * Redistributions in binary form must reproduce the above copyright notice,
 *   this list of conditions and the following disclaimer in the documentation
 *   and/or other materials provided with the distribution.
 *
 * * Neither the name of Google, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL GOOGLE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.SplittableRandom;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.concurrent.locks.LockSupport;

/**
 * HotSpot JIT stress for DynamoRIO: workloads that make HotSpot rewrite code
 * constantly (inline cache transitions, deoptimization, recompilation,
 * redefinition) while threads fault and are interrupted in that code.  Run by
 * jitstress.py, which compares the RESULT line with native and -Xint runs and
 * checks churn floors from -Xlog.  See README.md in this directory.
 *
 * Every scenario is bounded by operation counts, never by time; every result
 * that enters the checksum is independent of scheduling and of which code
 * (interpreted, C1, C2, old or new) ran; every call is checked against a
 * model computed here, and failures are counted as mismatches.
 *
 *   java -javaagent:jitstress.jar -cp jitstress.jar JitStress
 *        -Dscenario=IcNull|DeoptCycle|JfrNpe|Redefine -Dseed=N -Dthreads=N -Dops=N
 *
 * Output: "RESULT v1 scenario=... seed=... ops=... expectedExceptions=...
 * mismatches=... checksum=..." (compared), "STATS ..." (telemetry, never
 * compared), "MISMATCH ..." per failed check (at most 20), and
 * "OK JitStress-<scenario> <checksum>" when there was no mismatch.
 */
public final class JitStress {
    static volatile Instrumentation inst;

    public static void premain(String args, Instrumentation i) { inst = i; }

    static final long M1 = 0xBF58476D1CE4E5B9L;

    static long mix(long h, long v) {
        h ^= v;
        h *= 0x9E3779B97F4A7C15L;
        return h ^ (h >>> 29);
    }

    /* Failures and statistics shared by all scenarios. */
    static final AtomicLong mismatches = new AtomicLong();
    static final AtomicLong expectedExceptions = new AtomicLong();
    static final StringBuilder stats = new StringBuilder();

    static void mismatch(String what) {
        if (mismatches.incrementAndGet() <= 20) {
            synchronized (System.out) { System.out.println("MISMATCH " + what); }
        }
    }

    static void stat(String k, long v) { stats.append(' ').append(k).append('=').append(v); }

    static void join(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) t.join();
    }

    static int prop(String k, int def) { return Integer.getInteger(k, def); }

    public static void main(String[] args) throws Exception {
        String scenario = System.getProperty("scenario", "IcNull");
        long seed = Long.getLong("seed", 1);
        int scale = prop("scale", 1);
        long checksum;
        long ops;
        switch (scenario) {
            case "IcNull" -> { ops = prop("ops", 200_000) * (long) scale; checksum = IcNull.run(seed, prop("threads", 4), ops); }
            case "DeoptCycle" -> { ops = prop("ops", 100_000) * (long) scale; checksum = DeoptCycle.run(seed, prop("threads", 3), ops); }
            case "JfrNpe" -> { ops = prop("ops", 400_000) * (long) scale; checksum = JfrNpe.run(seed, prop("threads", 3), ops); }
            case "Redefine" -> { ops = prop("ops", 40) * (long) scale; checksum = Redefine.run(seed, ops); }
            default -> {
                System.out.println("CONFIGURATION_ERROR unknown scenario " + scenario);
                System.exit(2);
                return;
            }
        }
        System.out.printf(Locale.ROOT,
                          "RESULT v1 scenario=%s seed=%d ops=%d expectedExceptions=%d mismatches=%d checksum=%016x%n",
                          scenario, seed, ops, expectedExceptions.get(), mismatches.get(), checksum);
        System.out.println("STATS scenario=" + scenario + stats);
        if (mismatches.get() == 0) System.out.printf(Locale.ROOT, "OK JitStress-%s %016x%n", scenario, checksum);
        System.out.flush();
        System.exit(mismatches.get() == 0 ? 0 : 1);
    }

    /* ****************************************************************
     * IcNull: inline cache transitions with null receivers faulting at the
     * callee's entry.  Each site class has one non-inlined interface call
     * (jitstress.py passes -XX:CompileCommand=dontinline for icsite and f) and
     * nothing else that can trap, so C2 leaves the receiver null check to the
     * callee's inline cache check (an implicit null check: SIGSEGV).  Per
     * phase the receivers at a site are one class (monomorphic), two
     * (bimorphic) or all of them, including hidden classes a definer thread
     * keeps defining and dropping (megamorphic; class unloading cleans the
     * inline caches, which then go through the states again).  Null receivers
     * are predetermined by each worker's seed after a warm-up.  All receivers
     * compute spec(x).
     */
    public interface Recv { long f(long x); }

    static long spec(long x) {
        long y = (x ^ (x >>> 31)) * M1;
        return y ^ (y >>> 27);
    }

    // Equivalent bodies with different code: ((a + K) * M1 - K * M1) == a * M1.
    static final class R0 implements Recv { public long f(long x) { long y = (x ^ (x >>> 31)) * M1; return y ^ (y >>> 27); } }
    static final class R1 implements Recv { public long f(long x) { long y = ((x ^ (x >>> 31)) + 1) * M1 - M1; return y ^ (y >>> 27); } }
    static final class R2 implements Recv { public long f(long x) { long y = ((x >>> 31) ^ x) * M1; return (y >>> 27) ^ y; } }
    static final class R3 implements Recv { public long f(long x) { long y = ((x ^ (x >>> 31)) + 3) * M1 - 3 * M1; return y ^ (y >>> 27); } }
    static final class R4 implements Recv { public long f(long x) { long a = x >>> 31; long y = (a ^ x) * M1; return y ^ (y >>> 27); } }
    static final class R5 implements Recv { public long f(long x) { long y = ((x ^ (x >>> 31)) + 5) * M1 - 5 * M1; return y ^ (y >>> 27); } }
    static final class R6 implements Recv { public long f(long x) { long y = (x ^ (x >>> 31)) * M1; long z = y >>> 27; return z ^ y; } }
    static final class R7 implements Recv { public long f(long x) { long y = ((x ^ (x >>> 31)) + 7) * M1 - 7 * M1; return y ^ (y >>> 27); } }

    static final class S0 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S1 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S2 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S3 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S4 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S5 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S6 { static long icsite(Recv r, long x) { return r.f(x); } }
    static final class S7 { static long icsite(Recv r, long x) { return r.f(x); } }

    static final class IcNull {
        static final int NSITE = 8, NFIXED = 8, NHIDDEN = 8;
        static final Recv[] fixed = { new R0(), new R1(), new R2(), new R3(), new R4(), new R5(), new R6(), new R7() };
        /* Hidden receivers, replaced by the definer; a null slot falls back to a fixed one. */
        static final AtomicReferenceArray<Recv> hidden = new AtomicReferenceArray<>(NHIDDEN);
        static final AtomicLong defined = new AtomicLong(), gcs = new AtomicLong();
        static volatile boolean done;
        static final long NPE_MARK = 0x4E50455F4E504521L;

        static long call(int site, Recv r, long x) {
            return switch (site) {
                case 0 -> S0.icsite(r, x);
                case 1 -> S1.icsite(r, x);
                case 2 -> S2.icsite(r, x);
                case 3 -> S3.icsite(r, x);
                case 4 -> S4.icsite(r, x);
                case 5 -> S5.icsite(r, x);
                case 6 -> S6.icsite(r, x);
                default -> S7.icsite(r, x);
            };
        }

        /* The class file of JitStressHiddenRecv, defined again and again as a
         * new hidden class (no class file API, so this runs on JDK 21 too). */
        static byte[] template;

        static Recv defineHidden() throws Exception {
            if (template == null) {
                try (InputStream in = JitStress.class.getResourceAsStream("/JitStressHiddenRecv.class")) {
                    template = in.readAllBytes();
                }
            }
            MethodHandles.Lookup l = MethodHandles.lookup().defineHiddenClass(template, true);
            return (Recv) l.lookupClass().getDeclaredConstructor().newInstance();
        }

        static Recv pick(int site, int phase, SplittableRandom r) {
            switch (phase) {
                case 0: return fixed[site];                              // monomorphic
                case 1: return fixed[(site + r.nextInt(2)) % NFIXED];   // bimorphic
                case 2: {                                                // megamorphic
                    int k = r.nextInt(NFIXED + NHIDDEN);
                    if (k < NFIXED) return fixed[k];
                    Recv h = hidden.get(k - NFIXED);
                    return h != null ? h : fixed[k % NFIXED];
                }
                default: return fixed[(site + 3) % NFIXED];              // monomorphic again
            }
        }

        static long run(long seed, int threads, long ops) throws Exception {
            for (int i = 0; i < NHIDDEN; i++) hidden.set(i, defineHidden());
            defined.set(NHIDDEN);
            Thread definer = new Thread(() -> {
                try {
                    while (!done) {
                        for (int i = 0; i < NHIDDEN && !done; i++) {
                            hidden.set(i, defineHidden());
                            defined.incrementAndGet();
                            Thread.yield();
                        }
                        System.gc();  // unloads dropped hidden classes: cleans inline caches
                        gcs.incrementAndGet();
                    }
                } catch (Exception e) {
                    mismatch("definer: " + e);
                }
            }, "jitstress-definer");
            definer.setDaemon(true);
            definer.start();
            long[] h = new long[threads];
            long[] npes = new long[threads];
            Thread[] ts = new Thread[threads];
            long warm = Math.min(ops / 4, 20_000);
            long phaseLen = Math.max(1, ops / 16);
            for (int t = 0; t < threads; t++) {
                final int id = t;
                ts[t] = new Thread(() -> {
                    SplittableRandom r = new SplittableRandom(seed * 1000003 + id);
                    long acc = id, npe = 0;
                    for (long i = 0; i < ops; i++) {
                        int site = r.nextInt(NSITE);
                        int phase = (int) ((i / phaseLen + id) & 3);
                        Recv rv = pick(site, phase, r);
                        boolean isNull = i >= warm && r.nextInt(16) == 0;
                        long x = r.nextLong();
                        long v, want;
                        try {
                            v = call(site, isNull ? null : rv, x);
                        } catch (NullPointerException e) {
                            v = NPE_MARK ^ x;
                            npe++;
                        }
                        want = isNull ? NPE_MARK ^ x : spec(x);
                        if (v != want)
                            mismatch("IcNull thread " + id + " op " + i + " site " + site + " receiver "
                                     + (isNull ? "null" : rv.getClass().getName()) + " x " + x + ": got " + v
                                     + " expected " + want);
                        acc = mix(acc, v);
                    }
                    h[id] = acc;
                    npes[id] = npe;
                }, "jitstress-ic-" + t);
                ts[t].start();
            }
            join(ts);
            done = true;
            definer.join();
            long sum = 0, n = 0;
            for (int t = 0; t < threads; t++) { sum = mix(sum, h[t]); n += npes[t]; }
            expectedExceptions.addAndGet(n);
            stat("threads", threads);
            stat("hiddenDefined", defined.get());
            stat("gcs", gcs.get());
            return sum;
        }
    }

    /* ****************************************************************
     * DeoptCycle: chains of small methods carrying live ints and objects
     * through uncommon traps whose outcome flips in bursts: unstable_if
     * (mode 1), range_check (2: ArrayIndexOutOfBounds), class_check (3:
     * ClassCastException), null_check (4: NullPointerException), and a third
     * Shape subclass loaded half way (class hierarchy dependency).  With
     * jitstress.py's unlimited-trap flags each burst deoptimizes and the
     * methods are recompiled again and again; values rematerialized by deopt
     * (the scalar-replaced Box, the live longs) are part of every result.
     */
    static final class Box {
        long v;
        Box(long v) { this.v = v; }
    }

    abstract static class Shape { abstract long w(long x); }
    static final class Sq extends Shape { long w(long x) { return x * x; } }
    static final class Tri extends Shape { long w(long x) { return x * 3 + 1; } }
    static final class Pent extends Shape { long w(long x) { return x * 5 - 2; } }

    static final class DeoptCycle {
        static final int[] ARR = { 11, 22, 33, 44, 55, 66, 77, 88 };

        static long dc1(long x, int mode, Object o, Shape s) {
            long a = x * 31 + 7;
            Box b = new Box(a ^ 0x5DEECE66DL);
            long r = dc2(a, mode, o, b);
            int idx = (int) (x & 7) + (mode == 2 ? 8 : 0);
            r += ARR[idx];
            r += s.w(x);
            return r ^ b.v;
        }

        static long dc2(long a, int mode, Object o, Box b) {
            long r = a;
            if (mode == 1) r += b.v >>> 3; else r -= 5;
            if (o instanceof Integer i) r += i;
            else r += ((String) o).length();
            return dc3(r, mode, o);
        }

        static long dc3(long r, int mode, Object o) {
            Object n = mode == 4 ? null : o;
            return r * 3 + n.hashCode();
        }

        static final long AIOOBE = 0xA100BEL, CCE = 0xCCEL, NPE = 0x4E5045L;

        /** dc1's result, or the exception's code, written without exceptions. */
        static long model(long x, int mode, Object o, Shape s) {
            long a = x * 31 + 7;
            long bv = a ^ 0x5DEECE66DL;
            long r = mode == 1 ? a + (bv >>> 3) : a - 5;
            if (o instanceof Integer i) r += i.intValue();
            else if (o instanceof String str) r += str.length();
            else return CCE ^ x;
            if (mode == 4) return NPE ^ x;
            r = r * 3 + o.hashCode();
            if (mode == 2) return AIOOBE ^ x;
            r += ARR[(int) (x & 7)];
            long w = s instanceof Sq ? x * x : s instanceof Tri ? x * 3 + 1 : x * 5 - 2;
            return (r + w) ^ bv;
        }

        static long run(long seed, int threads, long ops) throws Exception {
            long[] h = new long[threads];
            long[] exc = new long[threads];
            Thread[] ts = new Thread[threads];
            Shape sq = new Sq(), tri = new Tri();
            Object[] objs = { Integer.valueOf(12345), "jitstress", Integer.valueOf(-7), "deopt-cycle" };
            Object bad = Long.valueOf(99);
            CountDownLatch pentReady = new CountDownLatch(1);
            Shape[] pent = new Shape[1];
            for (int t = 0; t < threads; t++) {
                final int id = t;
                ts[t] = new Thread(() -> {
                    SplittableRandom r = new SplittableRandom(seed * 7919 + id);
                    long acc = id, ne = 0;
                    long burstEvery = 2000 + r.nextInt(2000);
                    for (long i = 0; i < ops; i++) {
                        // Every burstEvery ops, a burst of 50 in one trap mode.
                        long p = i % burstEvery;
                        int mode = p < 50 && i > burstEvery ? (int) (1 + (i / burstEvery) % 4) : 0;
                        long x = r.nextLong();
                        Object o = mode == 3 ? bad : objs[r.nextInt(objs.length)];
                        Shape s;
                        if (i == ops / 2 && id == 0) {
                            pent[0] = new Pent();  // loads Pent: CHA dependency change
                            pentReady.countDown();
                        }
                        if (i > ops / 2 && (i & 15) == 0) {
                            try { pentReady.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
                            s = pent[0];
                        } else s = (i & 1) == 0 ? sq : tri;
                        long v;
                        try {
                            v = dc1(x, mode, o, s);
                        } catch (ArrayIndexOutOfBoundsException e) {
                            v = AIOOBE ^ x;
                            ne++;
                        } catch (ClassCastException e) {
                            v = CCE ^ x;
                            ne++;
                        } catch (NullPointerException e) {
                            v = NPE ^ x;
                            ne++;
                        }
                        long want = model(x, mode, o, s);
                        if (v != want)
                            mismatch("DeoptCycle thread " + id + " op " + i + " mode " + mode + " x " + x + " o "
                                     + o + " shape " + s.getClass().getSimpleName() + ": got " + v + " expected "
                                     + want);
                        acc = mix(acc, v);
                    }
                    h[id] = acc;
                    exc[id] = ne;
                }, "jitstress-deopt-" + t);
                ts[t].start();
            }
            join(ts);
            long sum = 0, n = 0;
            for (int t = 0; t < threads; t++) { sum = mix(sum, h[t]); n += exc[t]; }
            expectedExceptions.addAndGet(n);
            stat("threads", threads);
            return sum;
        }
    }

    /* ****************************************************************
     * JfrNpe (after Seedbox's Jfr probe, "exec 1 npe", which found the
     * elided-call bug with ZGC): JFR execution sampling every millisecond
     * suspends threads at arbitrary JIT pcs while they take implicit null
     * checks.  jitstress.py runs it with ZGC.
     */
    static final class JfrNpe {
        static volatile long sink;

        static long step(long x, long i, Object lock) {
            Object o = (i & 7) == 0 ? null : lock;
            try {
                x += o.hashCode() == 0 ? 1 : 2;
            } catch (NullPointerException e) {
                x += 3;
            }
            return x * 6364136223846793005L + 1442695040888963407L;
        }

        static long model(long x, long i) {
            x += (i & 7) == 0 ? 3 : 2;  // an identity hash is never 0
            return x * 6364136223846793005L + 1442695040888963407L;
        }

        static long run(long seed, int threads, long ops) throws Exception {
            Path file = Files.createTempFile("jitstress", ".jfr");
            long[] res = new long[threads];
            Object lock = new Object();
            long samples = 0;
            try (jdk.jfr.Recording rec = new jdk.jfr.Recording()) {
                rec.enable("jdk.ExecutionSample").withPeriod(Duration.ofMillis(1));
                rec.enable("jdk.NativeMethodSample").withPeriod(Duration.ofMillis(1));
                rec.setMaxSize(16L << 20);
                rec.start();
                Thread[] ts = new Thread[threads];
                for (int t = 0; t < threads; t++) {
                    final int id = t;
                    ts[t] = new Thread(() -> {
                        long x = seed + id, m = seed + id;
                        for (long i = 0; i < ops; i++) {
                            x = step(x, i, lock);
                            m = model(m, i);
                            if ((i & 4095) == 0) synchronized (lock) { sink += x; }
                            if ((i & 65535) == 0) Thread.yield();
                        }
                        if (x != m) mismatch("JfrNpe thread " + id + ": got " + x + " expected " + m);
                        res[id] = x;
                    }, "jitstress-jfr-" + t);
                    ts[t].start();
                }
                join(ts);
                rec.stop();
                rec.dump(file);
            }
            for (jdk.jfr.consumer.RecordedEvent e : jdk.jfr.consumer.RecordingFile.readAllEvents(file)) {
                if (e.getEventType().getName().equals("jdk.ExecutionSample")) samples++;
            }
            Files.delete(file);
            long sum = 0;
            for (int t = 0; t < threads; t++) sum = mix(sum, res[t]);
            expectedExceptions.addAndGet(threads * ((ops + 7) / 8));
            stat("threads", threads);
            stat("executionSamples", samples);
            return sum;
        }
    }

    /* ****************************************************************
     * Redefine (after Seedbox's RedefineWrongMethod and RedefineOldFrames):
     * a hot thread calls JitStressRTarget.capture in compiled code while the
     * main thread redefines the class between two versions (a constant
     * differs) with threads parked inside it: obsolete frames must finish with
     * their own version; calls that start after a redefinition returned must
     * see the new one.  Needs the agent (jitstress.py passes -javaagent).
     */
    static class Gate { long await(long pre) { return pre ^ 0x12345678L; } }

    static final class Park extends Gate {
        final CountDownLatch arrived = new CountDownLatch(1), release = new CountDownLatch(1);

        @Override
        long await(long pre) {
            arrived.countDown();
            try { release.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
            return pre ^ 0x12345678L;
        }
    }

    static final class Redefine {
        static final int A = 0x5EEDA0A1, B = 0x5EEDB0B1;

        static long model(int v, int depth, long x) {
            long pre = x * 31 + v + v;
            long r = depth > 0 ? model(v, depth - 1, x + 1) : pre ^ 0x12345678L;
            return r * 7 + pre + v;
        }

        /** Replaces the version constant (a CONSTANT_Integer and the folded CONSTANT_Long). */
        static byte[] patch(byte[] in, int from, int to) {
            byte[] out = in.clone();
            int hits = 0;
            for (int i = 0; i + 9 < out.length; i++) {
                int at = out[i] == 3 ? i + 1
                    : out[i] == 5 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 0 && out[i + 4] == 0 ? i + 5
                    : -1;
                if (at > 0 && (out[at] & 0xff) == (from >>> 24) && (out[at + 1] & 0xff) == ((from >>> 16) & 0xff)
                    && (out[at + 2] & 0xff) == ((from >>> 8) & 0xff) && (out[at + 3] & 0xff) == (from & 0xff)) {
                    out[at] = (byte) (to >>> 24);
                    out[at + 1] = (byte) (to >>> 16);
                    out[at + 2] = (byte) (to >>> 8);
                    out[at + 3] = (byte) to;
                    hits++;
                }
            }
            if (hits < 2) throw new IllegalStateException("version constant found " + hits + " times");
            return out;
        }

        record Parked(Thread thread, Park gate, int version, int depth, long x, long[] result) {}

        static Parked park(int version, int depth, long x) throws InterruptedException {
            Park gate = new Park();
            long[] result = { 0 };
            Thread t = new Thread(() -> result[0] = JitStressRTarget.capture(depth, x, gate), "jitstress-parked");
            t.start();
            gate.arrived.await();
            return new Parked(t, gate, version, depth, x, result);
        }

        static long release(Parked p) throws InterruptedException {
            p.gate.release.countDown();
            p.thread.join();
            long want = model(p.version, p.depth, p.x);
            if (p.result[0] != want)
                mismatch("Redefine: a frame parked in version " + Integer.toHexString(p.version) + " depth " + p.depth
                         + " returned " + p.result[0] + " expected " + want);
            return p.result[0];
        }

        static volatile int current = A;
        static volatile long epoch;
        static volatile boolean stop;
        static final AtomicLong hotCalls = new AtomicLong();

        static void redefine(Instrumentation inst, byte[] bytes, int version) throws Exception {
            epoch = epoch + 1;
            inst.redefineClasses(new ClassDefinition(JitStressRTarget.class, bytes));
            current = version;
            epoch = epoch + 1;
        }

        static void waitHot() throws InterruptedException {
            long target = hotCalls.get() + 200;
            while (hotCalls.get() < target && !stop) Thread.onSpinWait();
        }

        static long run(long seed, long rounds) throws Exception {
            Instrumentation inst = JitStress.inst;
            if (inst == null || !inst.isRedefineClassesSupported()) {
                System.out.println("CONFIGURATION_ERROR Redefine needs -javaagent:jitstress.jar");
                System.exit(2);
            }
            byte[] va;
            try (InputStream in = JitStress.class.getResourceAsStream("/JitStressRTarget.class")) {
                va = in.readAllBytes();
            }
            byte[] vb = patch(va, A, B);
            Gate plain = new Gate();
            long warm = 0;
            for (int i = 0; i < 40_000; i++) warm += JitStressRTarget.capture(i & 3, i, plain);
            Thread hot = new Thread(() -> {
                for (long i = 0; !stop; i++) {
                    long e1 = epoch;
                    int want = current;
                    int depth = (int) (i & 3);
                    long r = JitStressRTarget.capture(depth, i, plain);
                    // A call that a redefinition overlapped may mix versions (its
                    // older frames are obsolete, newer calls run the new code);
                    // one entirely after a completed redefinition may not.
                    if (e1 == epoch && (e1 & 1) == 0 && r != model(want, depth, i))
                        mismatch("Redefine: hot call " + i + " (depth " + depth + ") returned " + r
                                 + ", not version " + Integer.toHexString(want) + "'s result, after the redefinition"
                                 + " returned");
                    hotCalls.incrementAndGet();
                }
            }, "jitstress-hot");
            hot.start();
            SplittableRandom rnd = new SplittableRandom(seed);
            long h = 0, redefinitions = 0;
            for (long round = 0; round < rounds; round++) {
                int from = current, to = from == A ? B : A;
                byte[] toBytes = to == B ? vb : va, fromBytes = to == B ? va : vb;
                List<Parked> parked = new ArrayList<>();
                Parked across = park(from, 1 + rnd.nextInt(8), rnd.nextLong());
                for (int k = 0; k < 2; k++) parked.add(park(from, 1 + rnd.nextInt(8), rnd.nextLong()));
                redefine(inst, toBytes, to);
                redefinitions++;
                waitHot();
                for (Parked p : parked) h = mix(h, release(p));
                int d = rnd.nextInt(8);
                long x = rnd.nextLong();
                long fresh = JitStressRTarget.capture(d, x, Redefine.plainGate);
                if (fresh != model(to, d, x)) mismatch("Redefine: a fresh call after the redefinition ran the old version");
                h = mix(h, fresh);
                for (int i = 0; i < 5_000; i++) warm += JitStressRTarget.capture(i & 3, i, plain);
                Parked back = park(to, 1 + rnd.nextInt(8), rnd.nextLong());
                redefine(inst, fromBytes, from);
                redefinitions++;
                waitHot();
                h = mix(h, release(back));
                h = mix(h, release(across));  // started in `from`, lived through `to`, released in `from`
            }
            stop = true;
            hot.join();
            stat("redefinitions", redefinitions);
            stat("hotCalls", hotCalls.get());
            return mix(h, redefinitions);
        }

        static final Gate plainGate = new Gate();
    }
}

/** Defined repeatedly as a hidden class by IcNull's definer thread. */
final class JitStressHiddenRecv implements JitStress.Recv {
    public long f(long x) {
        long y = ((x >>> 31) ^ x) * JitStress.M1;
        return (y >>> 27) ^ y;
    }
}

/** Redefined by JitStress's Redefine scenario: 0x5EEDA0A1 becomes 0x5EEDB0B1 and back. */
final class JitStressRTarget {
    static int k() { return 0x5EEDA0A1; }

    static long capture(int depth, long x, JitStress.Gate gate) {
        long pre = x * 31 + 0x5EEDA0A1 + k();
        long r = depth > 0 ? capture(depth - 1, x + 1, gate) : gate.await(pre);
        return r * 7 + pre + 0x5EEDA0A1;
    }
}
