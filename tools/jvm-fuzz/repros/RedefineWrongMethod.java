import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.concurrent.CountDownLatch;

/**
 * JVM crash under the translator, reduced from the RedefineOldFrames probe: a thread calls a
 * recursive static method of RwmTarget in compiled code while the main thread redefines
 * RwmTarget between two versions (one constant differs) and other threads park inside it. After
 * a redefinition, the caller reaches the made-not-entrant callee through wrong_method_stub, and
 * SharedRuntime::find_callee_method walks a corrupt caller frame:
 *
 *   SIGSEGV (0xb) ... si_addr: 0x006b500000000882
 *   V  [libjvm.so+0x5d4be8]  vframeStream::vframeStream(JavaThread*, bool, bool, bool)+0x348
 *   V  [libjvm.so+0xc7eadc]  SharedRuntime::find_callee_method(JavaThread*)+0x4c
 *   V  [libjvm.so+0xc7ef44]  SharedRuntime::reresolve_call_site(JavaThread*)+0x224
 *   V  [libjvm.so+0xc7f190]  SharedRuntime::handle_wrong_method(JavaThread*)+0xcc
 *   v  ~RuntimeStub::wrong_method_stub
 *   C  0x00000000e817c998      <- a heap address where a return address should be
 *
 * Natively it always prints "OK". Build with Agent.java (../probes) into agent.jar as the
 * corpus image does, then:
 *   java -Xmx384m -Xss1m -javaagent:agent.jar -cp . RedefineWrongMethod [ROUNDS [nopark]]
 * under drrun -opt_speed -disable_traces -ldstex2cas -no_hw_cache_consistency -fake_ctr_dic.
 * Observed (one CPU, 2 GiB): native 0 of 11 crash; release translator 3 of 6; debug translator
 * (-debug, no DR assertion before the crash) 3 of 3; with "nopark" (no thread parked inside
 * RwmTarget during the redefinition) 0 of 6.
 */
public final class RedefineWrongMethod {
    static final int A = 0x5EEDA0A1, B = 0x5EEDB0B1;

    static class Gate {
        long await(long pre) { return pre ^ 0x12345678L; }
    }

    static final class Park extends Gate {
        final CountDownLatch arrived = new CountDownLatch(1), release = new CountDownLatch(1);

        @Override
        long await(long pre) {
            arrived.countDown();
            try { release.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
            return pre ^ 0x12345678L;
        }
    }

    /** Replaces the constant, kept by javac as a CONSTANT_Integer and a CONSTANT_Long. */
    static byte[] patch(byte[] bytes, int from, int to) {
        byte[] out = bytes.clone();
        for (int i = 0; i + 8 < out.length; i++) {
            int at = out[i] == 3 ? i + 1 : out[i] == 5 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 0
                && out[i + 4] == 0 ? i + 5 : -1;
            if (at > 0 && out[at] == (byte) (from >>> 24) && out[at + 1] == (byte) (from >>> 16)
                && out[at + 2] == (byte) (from >>> 8) && out[at + 3] == (byte) from) {
                out[at] = (byte) (to >>> 24);
                out[at + 1] = (byte) (to >>> 16);
                out[at + 2] = (byte) (to >>> 8);
                out[at + 3] = (byte) to;
            }
        }
        return out;
    }

    static volatile boolean stop;

    public static void main(String[] args) throws Exception {
        Instrumentation inst = Agent.inst;
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        boolean park = !(args.length > 1 && args[1].equals("nopark"));
        byte[] va;
        try (InputStream in = RedefineWrongMethod.class.getResourceAsStream("/RwmTarget.class")) { va = in.readAllBytes(); }
        byte[] vb = patch(va, A, B);
        Gate plain = new Gate();
        long sink = 0;
        for (int i = 0; i < 40_000; i++) sink += RwmTarget.capture(i & 3, i, plain);
        Thread hot = new Thread(() -> {
            long s = 0;
            for (long i = 0; !stop; i++) s += RwmTarget.capture((int) (i & 3), i, plain);
            System.out.println(s == 42 ? "" : "hot done");
        }, "hot");
        hot.start();
        for (int r = 0; r < rounds; r++) {
            if (park) {
                Park gate = new Park();
                Thread parked = new Thread(() -> RwmTarget.capture(1 + (int) (System.nanoTime() & 7), 1, gate));
                parked.start();
                gate.arrived.await();
                inst.redefineClasses(new ClassDefinition(RwmTarget.class, (r & 1) == 0 ? vb : va));
                gate.release.countDown();
                parked.join();
            } else {
                inst.redefineClasses(new ClassDefinition(RwmTarget.class, (r & 1) == 0 ? vb : va));
            }
            Thread.sleep(2);
        }
        stop = true;
        hot.join();
        System.out.println("OK " + rounds + " redefinitions " + (sink != 0));
    }
}

final class RwmTarget {
    static int k() { return 0x5EEDA0A1; }

    static long capture(int depth, long x, RedefineWrongMethod.Gate gate) {
        long pre = x * 31 + 0x5EEDA0A1 + k();
        long r = depth > 0 ? capture(depth - 1, x + 1, gate) : gate.await(pre);
        return r * 7 + pre + 0x5EEDA0A1;
    }
}
