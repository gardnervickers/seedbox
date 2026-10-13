import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Redefinition with old frames on the stack (through the agent jar, like Redefine). A thread
 * parks deep inside warmed, recursive RofTarget.capture frames after they captured version A's
 * constant; the main thread redefines RofTarget to B (which changes that constant), releases the
 * parked frames and requires A's result from them: obsolete frames keep executing their own
 * bytecode. Once redefineClasses has returned, a hot caller must see B on every fresh call.
 * Rounds alternate A to B and back, with a no-op retransformation between, nested frames of
 * seeded depth, and one thread parked across both redefinitions of a round.
 */
public final class RedefineOldFrames {
    static final int A = 0x5EEDA0A1, B = 0x5EEDB0B1;

    /** Where capture parks at its deepest frame; the plain gate returns at once. */
    static class Gate {
        long await(long pre) { return pre ^ 0x1234_5678L; }
    }

    static final class Park extends Gate {
        final CountDownLatch arrived = new CountDownLatch(1), release = new CountDownLatch(1);

        @Override
        long await(long pre) {
            arrived.countDown();
            try {
                release.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            return pre ^ 0x1234_5678L;
        }
    }

    /** RofTarget.capture with version constant v, computed here independently. */
    static long model(int v, int depth, long x) {
        long pre = x * 31 + v + v;
        long r = depth > 0 ? model(v, depth - 1, x + 1) : pre ^ 0x1234_5678L;
        return r * 7 + pre + v;
    }

    /** Replaces the version constant, which javac keeps as a CONSTANT_Integer and a CONSTANT_Long. */
    static byte[] patch(byte[] bytes, int from, int to) {
        byte[] out = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 8 < out.length; i++) {
            int at = out[i] == 3 ? i + 1 : out[i] == 5 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 0
                && out[i + 4] == 0 ? i + 5 : -1;
            if (at > 0 && out[at] == (byte) (from >>> 24) && out[at + 1] == (byte) (from >>> 16)
                && out[at + 2] == (byte) (from >>> 8) && out[at + 3] == (byte) from) {
                out[at] = (byte) (to >>> 24);
                out[at + 1] = (byte) (to >>> 16);
                out[at + 2] = (byte) (to >>> 8);
                out[at + 3] = (byte) to;
                hits++;
            }
        }
        Fz.expect(hits == 2, "two patchable constants, found " + hits);
        return out;
    }

    record Parked(Thread thread, Park gate, int version, int depth, long x, long[] result) {}

    static Parked park(int version, int depth, long x) throws InterruptedException {
        Park gate = new Park();
        long[] result = {0};
        Thread t = new Thread(() -> result[0] = RofTarget.capture(depth, x, gate), "fz-parked");
        t.start();
        gate.arrived.await();
        return new Parked(t, gate, version, depth, x, result);
    }

    /** Releases a parked thread; its frames must finish with the version they started with. */
    static boolean release(Parked p) throws InterruptedException {
        p.gate.release.countDown();
        p.thread.join();
        Fz.mix(p.result[0]);
        return p.result[0] == model(p.version, p.depth, p.x);
    }

    // The loaded version, and an epoch that is odd while a redefinition is in progress: a call
    // made entirely within one even epoch must return that epoch's version.
    static volatile int current = A;
    static volatile long epoch = 0;
    static final AtomicLong staleFresh = new AtomicLong(), freshCalls = new AtomicLong();

    public static void main(String[] args) throws Exception {
        Fz.init("RedefineOldFrames", args);
        Instrumentation inst = Agent.inst;
        Fz.expect(inst != null && inst.isRedefineClassesSupported() && inst.isRetransformClassesSupported(),
                  "agent with redefinition");
        byte[] va;
        try (InputStream in = RedefineOldFrames.class.getResourceAsStream("/RofTarget.class")) { va = in.readAllBytes(); }
        byte[] vb = patch(va, A, B);
        inst.addTransformer(new ClassFileTransformer() {
            public byte[] transform(Module m, ClassLoader l, String name, Class<?> c, ProtectionDomain pd, byte[] b) {
                return null;
            }
        }, true);
        Gate plain = new Gate();
        // Warm capture through every depth so its frames are compiled before anything parks.
        long warm = 0;
        for (int i = 0; i < 40_000; i++) warm += RofTarget.capture(i & 3, i, plain);

        // A hot caller: a call that read `current` after a redefinition returned must see it.
        AtomicBoolean stop = new AtomicBoolean();
        Thread hot = new Thread(() -> {
            long i = 0;
            while (!stop.get()) {
                long e1 = epoch;
                int want = current;
                int depth = (int) (i & 3);
                long r = RofTarget.capture(depth, i, plain);
                if (e1 == epoch && (e1 & 1) == 0 && r != model(want, depth, i)) staleFresh.incrementAndGet();
                freshCalls.incrementAndGet();
                i++;
            }
        }, "fz-hot");
        hot.start();

        java.util.SplittableRandom rnd = Fz.rnd;
        int rounds = 30 * Fz.scale;
        long oldBad = 0, freshBad = 0, released = 0;
        int redefinitions = 0;
        for (int round = 0; round < rounds; round++) {
            // The version now loaded is `from`; the round goes from -> to -> from.
            int from = current, to = from == A ? B : A;
            byte[] toBytes = to == B ? vb : va, fromBytes = to == B ? va : vb;
            List<Parked> parked = new ArrayList<>();
            Parked longLived = park(from, 1 + rnd.nextInt(8), rnd.nextLong());
            for (int k = 0; k < 2; k++) parked.add(park(from, 1 + rnd.nextInt(8), rnd.nextLong()));
            redefine(inst, toBytes, to);
            redefinitions++;
            waitForFreshCalls();
            for (Parked p : parked) {
                if (!release(p)) oldBad++;
                released++;
            }
            int d = rnd.nextInt(8);
            long x = rnd.nextLong();
            if (RofTarget.capture(d, x, plain) != model(to, d, x)) freshBad++;
            // A frame parked across a no-op retransformation keeps its (equivalent) code.
            Parked across = park(to, 1 + rnd.nextInt(4), rnd.nextLong());
            inst.retransformClasses(RofTarget.class);
            if (!release(across)) oldBad++;
            released++;
            // Warm the new version, then park in it and go back.
            for (int i = 0; i < 5_000; i++) warm += RofTarget.capture(i & 3, i, plain);
            Parked back = park(to, 1 + rnd.nextInt(8), rnd.nextLong());
            redefine(inst, fromBytes, from);
            redefinitions++;
            waitForFreshCalls();
            if (!release(back)) oldBad++;
            // The long-lived frames started in `from`, lived through `to` and are released in `from`.
            if (!release(longLived)) oldBad++;
            released += 2;
            if (RofTarget.capture(d, x, plain) != model(from, d, x)) freshBad++;
        }
        stop.set(true);
        hot.join();
        Fz.out("redefinitions", redefinitions);
        Fz.out("oldFrames", released + " bad " + oldBad);
        Fz.out("freshBad", freshBad);
        Fz.out("staleFresh", staleFresh.get());
        Fz.mix(redefinitions);
        Fz.mix(warm);
        Fz.mix(released);
        Fz.mix(oldBad + freshBad * 3 + staleFresh.get() * 7);
        Fz.done();
    }

    static void redefine(Instrumentation inst, byte[] bytes, int version) throws Exception {
        epoch = epoch + 1;
        inst.redefineClasses(new ClassDefinition(RofTarget.class, bytes));
        current = version;
        epoch = epoch + 1;
    }

    /** Waits until the hot caller has made calls that started after the redefinition. */
    static void waitForFreshCalls() throws InterruptedException {
        long target = freshCalls.get() + 200;
        while (freshCalls.get() < target) Thread.sleep(1);
    }
}

/** Redefined by RedefineOldFrames: the constant 0x5EEDA0A1 becomes 0x5EEDB0B1 and back. */
final class RofTarget {
    static int k() { return 0x5EEDA0A1; }

    static long capture(int depth, long x, RedefineOldFrames.Gate gate) {
        long pre = x * 31 + 0x5EEDA0A1 + k();
        long r = depth > 0 ? capture(depth - 1, x + 1, gate) : gate.await(pre);
        return r * 7 + pre + 0x5EEDA0A1;
    }
}
