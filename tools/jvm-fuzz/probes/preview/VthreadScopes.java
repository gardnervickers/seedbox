import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.StructuredTaskScope;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * State that follows a virtual thread rather than its carrier: confined arenas used across
 * unmount/remount, scoped values restored while exceptions unwind and rebuilt in forked subtasks
 * with a tiny cache, structured-scope sibling cancellation, inheritable thread-local snapshots,
 * and live object references held by frozen continuations while the collector moves them.
 * Run with -Djdk.virtualThreadScheduler.parallelism=4 -Djdk.preserveScopedValueCache=false
 * -Djava.lang.ScopedValue.cacheSize=2.
 */
public final class VthreadScopes {
    static final ScopedValue<Integer> K1 = ScopedValue.newInstance();
    static final ScopedValue<Integer>[] KEYS = keys(5);

    @SuppressWarnings("unchecked")
    static ScopedValue<Integer>[] keys(int n) {
        ScopedValue<Integer>[] k = new ScopedValue[n];
        for (int i = 0; i < n; i++) k[i] = ScopedValue.newInstance();
        return k;
    }

    static void confinedArenas() throws Exception {
        int n = 8;
        AtomicLong sum = new AtomicLong();
        AtomicInteger wrongThread = new AtomicInteger();
        long[] seeds = new long[n];
        for (int i = 0; i < n; i++) seeds[i] = Fz.rnd.nextLong();
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = Thread.ofVirtual().name("fz-arena-" + t).start(() -> {
                try (Arena a = Arena.ofConfined()) {
                    MemorySegment m = a.allocate(256);
                    long v = seeds[id];
                    for (int i = 0; i < 32; i++) m.setAtIndex(ValueLayout.JAVA_LONG, i, v + i);
                    for (int r = 0; r < 200; r++) {
                        if ((r & 1) == 0) LockSupport.parkNanos(1);
                        else Thread.yield();
                        int slot = r & 31;
                        long got = m.getAtIndex(ValueLayout.JAVA_LONG, slot);
                        if (got != v + slot) throw new AssertionError("arena slot " + slot);
                        m.setAtIndex(ValueLayout.JAVA_LONG, slot, got * 3 - 2 * (v + slot));
                    }
                    if (id == 0) {
                        // Another virtual thread, perhaps on this same carrier, is not the owner.
                        Thread other = Thread.ofVirtual().start(() -> {
                            try {
                                m.get(ValueLayout.JAVA_LONG, 0);
                            } catch (WrongThreadException e) {
                                wrongThread.incrementAndGet();
                            }
                            try {
                                a.close();
                            } catch (WrongThreadException e) {
                                wrongThread.incrementAndGet();
                            }
                        });
                        other.join();
                    }
                    long s = 0;
                    for (int i = 0; i < 32; i++) s += m.getAtIndex(ValueLayout.JAVA_LONG, i);
                    sum.addAndGet(s);
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            });
        }
        Fz.join(ts);
        long want = 0;
        for (long s : seeds) for (int i = 0; i < 32; i++) want += s + i;
        Fz.out("confinedArenaSum", Long.toHexString(sum.get()));
        Fz.out("confinedArenaWrongThread", wrongThread.get());
        Fz.expect(sum.get() == want, "confined arena contents across remounts");
        Fz.expect(wrongThread.get() == 2, "non-owner refused");
        Fz.mix(sum.get());
    }

    /** Binds K1 to successive values `depth` deep and throws at `throwAt`, checking each restore. */
    static int nest(int depth, int throwAt, int base) throws Exception {
        if (depth == 0) return K1.get();
        int outer = K1.get();
        int inner;
        try {
            inner = ScopedValue.where(K1, base + depth).call(() -> {
                if (depth == throwAt) throw new IllegalStateException("d" + depth);
                return nest(depth - 1, throwAt, base);
            });
        } catch (IllegalStateException e) {
            inner = -depth;
        }
        if (K1.get() != outer) throw new AssertionError("binding not restored at depth " + depth);
        return inner + outer;
    }

    static void scopedExceptions() throws Exception {
        long sum = 0;
        int unboundAfter = 0;
        for (int i = 0; i < 2_000 * Fz.scale; i++) {
            int depth = 1 + Fz.rnd.nextInt(8), throwAt = Fz.rnd.nextInt(depth + 2), base = Fz.rnd.nextInt(1000);
            int v = ScopedValue.where(K1, base).call(() -> nest(depth, throwAt, base));
            sum += v;
            if (!K1.isBound()) unboundAfter++;
        }
        int idea = ScopedValue.where(K1, 7).call(() -> {
            try {
                ScopedValue.where(K1, 9).run(() -> { throw new IllegalStateException(); });
            } catch (IllegalStateException e) {
                // restored to 7
            }
            return K1.get();
        });
        Fz.out("scopedRestore", sum + " " + unboundAfter + " " + idea + " " + K1.isBound());
        Fz.expect(idea == 7 && !K1.isBound(), "binding restored after unwinding");
        Fz.mix(sum);
    }

    static void siblingCancellation() throws Exception {
        int rounds = 15 * Fz.scale, siblings = 4;
        int cleanedTotal = 0, failedWith = 0, successes = 0;
        for (int r = 0; r < rounds; r++) {
            CountDownLatch ready = new CountDownLatch(siblings);
            AtomicInteger cleaned = new AtomicInteger();
            String tag = "x" + r;
            try (var s = new StructuredTaskScope.ShutdownOnFailure()) {
                for (int i = 0; i < siblings; i++) {
                    final int k = i;
                    s.fork(() -> {
                        ready.countDown();
                        try {
                            if (k % 2 == 0) Thread.sleep(60_000);
                            else LockSupport.park();
                            while (!Thread.currentThread().isInterrupted()) LockSupport.park();
                        } finally {
                            cleaned.incrementAndGet();
                        }
                        return 0;
                    });
                }
                s.fork(() -> {
                    ready.await();
                    throw new IllegalStateException(tag);
                });
                s.join();
                if (s.exception().orElseThrow().getMessage().equals(tag)) failedWith++;
            }
            cleanedTotal += cleaned.get();
            try (var s = new StructuredTaskScope.ShutdownOnSuccess<Integer>()) {
                int winner = Fz.rnd.nextInt(3);
                for (int i = 0; i < 3; i++) {
                    final int k = i;
                    s.fork(() -> {
                        if (k != winner) Thread.sleep(60_000);
                        return k * 10;
                    });
                }
                s.join();
                if (s.result() == winner * 10) successes++;
            }
        }
        Fz.out("siblingCancel", failedWith + " cleaned=" + cleanedTotal + " success=" + successes);
        Fz.expect(failedWith == rounds && cleanedTotal == rounds * siblings && successes == rounds, "structured cancellation");
    }

    static int readAll() {
        int s = 0;
        for (int i = 0; i < KEYS.length; i++) s = s * 31 + KEYS[i].get();
        return s;
    }

    static void scopedInheritance() throws Exception {
        long total = 0, want = 0;
        for (int r = 0; r < 20 * Fz.scale; r++) {
            int[] vals = new int[KEYS.length];
            for (int i = 0; i < vals.length; i++) vals[i] = Fz.rnd.nextInt(100_000);
            ScopedValue.Carrier c = ScopedValue.where(KEYS[0], vals[0]);
            for (int i = 1; i < KEYS.length; i++) c = c.where(KEYS[i], vals[i]);
            int expected = 0;
            for (int v : vals) expected = expected * 31 + v;
            int finalExpected = expected;
            int got = c.call(() -> {
                try (var s = new StructuredTaskScope.ShutdownOnFailure()) {
                    List<StructuredTaskScope.Subtask<Integer>> subs = new ArrayList<>();
                    for (int k = 0; k < 4; k++) {
                        final int kk = k;
                        subs.add(s.fork(() -> {
                            Thread.sleep(1);
                            int a = readAll();
                            // A nested rebinding inside the child, then the inherited value again.
                            int b = ScopedValue.where(KEYS[kk], -1).call(VthreadScopes::readAll);
                            Thread.yield();
                            int again = readAll();
                            if (again != a || b == a) throw new AssertionError("scoped value cache");
                            return a;
                        }));
                    }
                    s.join().throwIfFailed();
                    int sum = 0;
                    for (var t : subs) sum += t.get() == finalExpected ? 1 : 0;
                    return sum;
                }
            });
            total += got;
            want += 4;
            Fz.mix(expected);
        }
        Fz.out("scopedInherited", total + "/" + want);
        Fz.expect(total == want, "subtasks inherit bindings");
    }

    static void inheritableSnapshot() throws Exception {
        InheritableThreadLocal<Integer> k = new InheritableThreadLocal<>();
        int start = Fz.rnd.nextInt(1000);
        k.set(start);
        int[] out = new int[3];
        Thread t = Thread.ofVirtual().unstarted(() -> {
            for (int i = 0; i < 300; i++) LockSupport.parkNanos(1);
            out[0] = k.get();
            k.set(-5);
        });
        Thread none = Thread.ofVirtual().inheritInheritableThreadLocals(false).unstarted(() -> out[1] = k.get() == null ? 1 : 0);
        Thread plat = Thread.ofPlatform().unstarted(() -> {
            LockSupport.parkNanos(1000);
            out[2] = k.get();
        });
        k.set(start + 99);
        t.start();
        none.start();
        plat.start();
        Fz.join(new Thread[] {t, none, plat});
        Fz.out("inheritableSnapshot", (out[0] - start) + " " + out[1] + " " + (out[2] - start) + " " + (k.get() - start));
        Fz.expect(out[0] == start && out[1] == 1 && out[2] == start && k.get() == start + 99, "inherited snapshot");
    }

    static CountDownLatch frozen, go;

    static long deep(int d, long salt) throws InterruptedException {
        byte[] x = new byte[1024];
        long[] y = new long[4];
        for (int i = 0; i < x.length; i++) x[i] = (byte) (salt * 31 + d * 7 + i);
        y[d & 3] = salt ^ d;
        Object box = new long[] {salt + d};
        long below;
        if (d == 0) {
            frozen.countDown();
            go.await();
            below = 0;
        } else {
            below = deep(d - 1, salt);
        }
        for (int i = 0; i < x.length; i++) if (x[i] != (byte) (salt * 31 + d * 7 + i)) throw new AssertionError("frame array moved badly");
        if (y[d & 3] != (salt ^ d) || ((long[]) box)[0] != salt + d) throw new AssertionError("frame reference");
        return below + x[d & 1023] + d;
    }

    static void frozenRoots() throws Exception {
        int n = 6, depth = 100;
        frozen = new CountDownLatch(n);
        go = new CountDownLatch(1);
        long[] salts = new long[n], out = new long[n];
        for (int i = 0; i < n; i++) salts[i] = Fz.rnd.nextLong();
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = Thread.ofVirtual().start(() -> {
                try {
                    out[id] = deep(depth, salts[id]);
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            });
        }
        frozen.await();
        List<byte[]> churn = new ArrayList<>();
        for (int g = 0; g < 5; g++) {
            for (int i = 0; i < 2000; i++) churn.add(new byte[256 + (i & 511)]);
            if (g % 2 == 0) churn.clear();
            System.gc();
        }
        go.countDown();
        Fz.join(ts);
        long sum = 0, want = 0;
        for (int t = 0; t < n; t++) {
            sum += out[t];
            for (int d = 0; d <= depth; d++) want += (byte) (salts[t] * 31 + d * 7 + (d & 1023)) + d;
        }
        Fz.out("frozenRoots", Long.toHexString(sum));
        Fz.expect(sum == want, "frozen continuation roots");
        Fz.mix(sum);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("VthreadScopes", args);
        confinedArenas();
        scopedExceptions();
        siblingCancellation();
        scopedInheritance();
        inheritableSnapshot();
        frozenRoots();
        Fz.done();
    }
}
