import java.lang.invoke.MethodHandles;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.LongUnaryOperator;

/**
 * Signal chaining through libjsig: the probe starts itself again as a child JVM with
 * LD_PRELOAD=libjsig.so and the same JVM flags. In the child, the JNI helper (libfzjni.so)
 * installs a SIGSEGV handler with sigaction after HotSpot's; libjsig keeps HotSpot's handler and
 * chains to the helper's for faults HotSpot does not recognize. The main thread alternates native
 * faults on protected pages (the handler makes the page writable and the store retries), faults
 * whose load the handler skips by rewriting x0 and the PC, and faults from pthreads the JVM does
 * not know, with compiled arithmetic; another thread throws implicit NullPointerExceptions from
 * compiled code (HotSpot's own SIGSEGV path) and a third keeps compiling and dropping hidden
 * classes. Every handler path is counted and every resumed result is checked against a model.
 */
public final class LibjsigChaining {
    static native int install();
    static native long faultLoop(int n, long seed);
    static native long skipLoop(int n, long seed);
    static native long foreignFaults(int threads, int n, long seed);
    static native long count(int path);

    static long xs(long x) {
        x ^= x << 13;
        x ^= x >>> 7;
        x ^= x << 17;
        return x;
    }

    static long faultModel(int n, long seed) {
        long x = seed, acc = 0;
        for (int i = 0; i < n; i++) {
            x = xs(x);
            acc = acc * 31 + x;
        }
        return acc;
    }

    static long skipModel(int n, long seed) {
        long x = seed | 1, acc = 0;
        for (int i = 0; i < n; i++) {
            x = xs(x);
            acc = acc * 31 + (0x5EED5EED00000000L | (long) (4 + (i & 1)) << 16 | (x & 511) * 8);
        }
        return acc;
    }

    static long foreignModel(int threads, int n, long seed) {
        long acc = 0;
        for (int i = 0; i < threads; i++) acc = acc * 0x100000001B3L ^ faultModel(n, seed + i * 0x9E37L + 1);
        return acc;
    }

    static final class Box {
        int v;
        Box(int v) { this.v = v; }
    }

    static int load(Box b) { return b.v; }
    static int loadTwice(Box b, int k) { return b.v * k + b.v; }

    /** Implicit null checks in compiled code: a seeded fraction of the boxes are null. */
    static long npes(long seed, int iters, long[] thrown) {
        java.util.SplittableRandom r = new java.util.SplittableRandom(seed);
        Box[] boxes = new Box[512];
        for (int i = 0; i < boxes.length; i++) boxes[i] = r.nextInt(64) == 0 ? null : new Box(r.nextInt());
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            Box b = boxes[i & 511];
            try {
                acc += (i & 1) == 0 ? load(b) : loadTwice(b, i);
            } catch (NullPointerException e) {
                thrown[0]++;
                acc ^= i;
            }
        }
        return acc;
    }

    static long arith(long seed, int iters) {
        long h = seed, acc = 0;
        for (int i = 0; i < iters; i++) {
            h = h * 6364136223846793005L + 1442695040888963407L;
            int d = (int) (h >>> 60) - 3;
            try {
                acc += (h >> 7) / d + (h % (d | 1)) + Long.rotateLeft(h, i & 63);
            } catch (ArithmeticException e) {
                acc ^= 0x5EED;
            }
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 2 && args[2].equals("child")) {
            child(args);
            return;
        }
        // Parent: start the child with libjsig preloaded and the same flags; its output is ours.
        String home = System.getProperty("java.home");
        List<String> cmd = new ArrayList<>();
        cmd.add(Path.of(home, "bin", "java").toString());
        cmd.addAll(ManagementFactory.getRuntimeMXBean().getInputArguments());
        cmd.addAll(List.of("-cp", System.getProperty("java.class.path"), "LibjsigChaining",
                           args.length > 0 ? args[0] : "1", args.length > 1 ? args[1] : "1", "child"));
        ProcessBuilder pb = new ProcessBuilder(cmd).inheritIO();
        pb.environment().put("LD_PRELOAD", Path.of(home, "lib", "libjsig.so").toString());
        System.exit(pb.start().waitFor());
    }

    static void child(String[] args) throws Exception {
        Fz.init("LibjsigChaining", args);
        boolean jsig = Files.readString(Path.of("/proc/self/maps")).contains("libjsig.so");
        Fz.out("libjsig", jsig);
        Fz.expect(jsig, "libjsig.so preloaded");
        System.load(System.getProperty("fz.jnilib", "/fuzz/bin/libfzjni.so"));
        Fz.expect(install() == 0, "sigaction through libjsig");

        AtomicBoolean stop = new AtomicBoolean();
        Thread churn = new Thread(() -> {
            try (java.io.InputStream in = Fz.class.getResourceAsStream("/Payload.class")) {
                byte[] bytes = in.readAllBytes();
                long s = 0;
                while (!stop.get()) {
                    Class<?> c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                    var op = (LongUnaryOperator) c.getDeclaredConstructor().newInstance();
                    for (int i = 0; i < 20_000; i++) s += op.applyAsLong(i);
                    Thread.sleep(3);
                }
                if (s == 42) System.out.print("");
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        }, "fz-churn");
        churn.setDaemon(true);
        churn.start();

        int rounds = 160 * Fz.scale;
        long[] npeThrown = {0};
        long[] npeAcc = {0};
        Thread npe = new Thread(() -> {
            for (int k = 0; k < rounds; k++) npeAcc[0] = npeAcc[0] * 31 + npes(Fz.seed + k, 4000, npeThrown);
        }, "fz-npe");
        npe.start();

        long faults = 0, skips = 0, foreign = 0, bad = 0, acc = 0, mainNpe[] = {0};
        for (int k = 0; k < rounds; k++) {
            long s = Fz.seed * 1_000_003L + k;
            switch (k % 4) {
                case 0 -> {
                    int n = 8 + (int) (s & 15);
                    long r = faultLoop(n, s);
                    if (r != faultModel(n, s | 1)) bad++;
                    faults += n;
                    acc = acc * 31 + r;
                }
                case 1 -> {
                    int n = 8 + (int) (s & 7);
                    long r = skipLoop(n, s);
                    if (r != skipModel(n, s)) bad++;
                    skips += n;
                    acc = acc * 31 + r;
                }
                case 2 -> {
                    long r = foreignFaults(2, 6, s);
                    if (r != foreignModel(2, 6, s)) bad++;
                    foreign += 12;
                    acc = acc * 31 + r;
                }
                default -> acc = acc * 31 + arith(s, 20_000) + npes(s, 2000, mainNpe);
            }
        }
        npe.join();
        stop.set(true);
        churn.join();
        Fz.out("repairedJava", count(0) + " of " + faults);
        Fz.out("repairedForeign", count(1) + " of " + foreign);
        Fz.out("skipped", count(2) + " of " + skips);
        Fz.out("badResults", bad);
        Fz.out("npeMain", mainNpe[0]);
        Fz.out("npeThread", npeThrown[0]);
        Fz.outHex("result", acc);
        Fz.outHex("npeResult", npeAcc[0]);
        Fz.mix(acc);
        Fz.mix(npeAcc[0]);
        Fz.mix(count(0) + count(1) * 3 + count(2) * 7);
        Fz.expect(count(0) == faults && count(1) == foreign && count(2) == skips && bad == 0, "handler paths");
        Fz.done();
    }
}
