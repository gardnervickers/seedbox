import java.util.SplittableRandom;

/** Shared probe helpers: seeded inputs, an order-sensitive hash and result lines. */
final class Fz {
    static long seed = 1;
    static int scale = 1;
    static SplittableRandom rnd = new SplittableRandom(1);
    private static long h = 0xcbf29ce484222325L;
    private static String name = "?";

    static void init(String probe, String[] args) {
        name = probe;
        seed = args.length > 0 ? Long.parseLong(args[0]) : 1;
        scale = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        rnd = new SplittableRandom(seed);
        if (Boolean.getBoolean("fz.jitnoise")) jitNoise();
    }

    static volatile long noiseSink;

    /**
     * gen --jit-noise: a daemon thread keeps defining, compiling and dropping hidden classes, so
     * code is written and invalidated while the probe runs. It prints nothing.
     */
    private static void jitNoise() {
        Thread t = new Thread(() -> {
            try (java.io.InputStream in = Fz.class.getResourceAsStream("/Payload.class")) {
                byte[] bytes = in.readAllBytes();
                long s = 0;
                while (true) {
                    Class<?> c = java.lang.invoke.MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                    var op = (java.util.function.LongUnaryOperator) c.getDeclaredConstructor().newInstance();
                    for (int i = 0; i < 20_000; i++) s += op.applyAsLong(i);
                    noiseSink = s;
                    Thread.sleep(20);
                }
            } catch (Exception e) {
                noiseSink = -1;
            }
        }, "fz-jit-noise");
        t.setDaemon(true);
        t.start();
    }

    static void mix(long v) {
        h ^= v;
        h *= 0x100000001b3L;
        h ^= h >>> 29;
    }

    static void mix(double d) { mix(Double.doubleToLongBits(d)); }

    static void mix(String s) { mix(s.hashCode()); mix(s.length()); }

    static void mix(byte[] b) { mix(java.util.Arrays.hashCode(b)); mix(b.length); }

    static long hash() { return h; }

    /** Prints one named, deterministic observation. */
    static void out(String key, Object value) { System.out.println(key + " " + value); }

    static void outHex(String key, long value) { out(key, Long.toHexString(value)); }

    static void expect(boolean ok, String what) {
        if (!ok) {
            System.out.println("FAIL " + name + " " + what);
            System.out.flush();
            System.exit(3);
        }
    }

    static void done() { System.out.println("OK " + name + " " + Long.toHexString(h)); }

    static int edgeInt() {
        int[] e = {0, 1, -1, 2, -2, 7, 31, 32, 33, 63, 64, Integer.MIN_VALUE, Integer.MAX_VALUE,
            Integer.MIN_VALUE + 1, 0x55555555, 0xAAAAAAAA, 0xFFFF, 0x10000};
        return rnd.nextInt(4) == 0 ? e[rnd.nextInt(e.length)] : rnd.nextInt();
    }

    static long edgeLong() {
        long[] e = {0, 1, -1, 63, 64, 65, Long.MIN_VALUE, Long.MAX_VALUE, Long.MIN_VALUE + 1,
            0xFFFFFFFFL, 0x100000000L, Integer.MIN_VALUE, Integer.MAX_VALUE};
        return rnd.nextInt(4) == 0 ? e[rnd.nextInt(e.length)] : rnd.nextLong();
    }

    static double edgeDouble() {
        double[] e = {0.0, -0.0, 1.0, -1.0, 0.5, Double.NaN, Double.POSITIVE_INFINITY,
            Double.NEGATIVE_INFINITY, Double.MIN_VALUE, Double.MAX_VALUE, Double.MIN_NORMAL,
            1e308, -1e-308, Math.PI, 2147483648.0, -9.223372036854775808E18, 4.5, -2.5};
        return rnd.nextInt(3) == 0 ? e[rnd.nextInt(e.length)] : (rnd.nextDouble() - 0.5) * Math.scalb(1.0, rnd.nextInt(-60, 60));
    }

    static byte[] bytes(int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) b[i] = (byte) rnd.nextInt();
        return b;
    }

    static void join(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) t.join();
    }
}
