/** Tier transitions: OSR, class-hierarchy and type-profile deoptimization, implicit null checks. */
public final class JitDeopt {
    interface Shape { long area(long x); }
    static final class Sq implements Shape { public long area(long x) { return x * x; } }
    static final class Tri implements Shape { public long area(long x) { return x * (x + 1) / 2; } }
    static final class Hex implements Shape { public long area(long x) { return 3 * x * x - 3 * x + 1; } }

    static abstract class Base { abstract int v(int x); }
    static final class OnlyOne extends Base { int v(int x) { return x + 1; } }

    static long sum(Shape s, long n) {
        long t = 0;
        for (long i = 0; i < n; i++) t += s.area(i & 1023);
        return t;
    }

    static int callBase(Base b, int x) { return b.v(x); }

    static int len(int[] a) { return a.length; }

    static int unbox(Integer i) { return i; }

    public static void main(String[] args) throws Exception {
        Fz.init("JitDeopt", args);
        long n = 200_000L * Fz.scale;
        // OSR then monomorphic, bimorphic and megamorphic call sites at one bytecode index.
        Fz.mix(sum(new Sq(), n));
        Fz.mix(sum(new Tri(), n / 4));
        Fz.mix(sum(new Hex(), n / 4));
        Fz.mix(sum(new Sq(), n));
        // Class hierarchy analysis: inlined single implementor, then a new subclass is loaded.
        Base one = new OnlyOne();
        long acc = 0;
        for (int i = 0; i < 300_000; i++) acc += callBase(one, i);
        Class<?> late = Class.forName("JitDeopt$Late");
        Base other = (Base) late.getDeclaredConstructor().newInstance();
        for (int i = 0; i < 300_000; i++) acc += callBase((i & 1) == 0 ? one : other, i);
        Fz.mix(acc);
        // Implicit null and range checks taken from compiled code, repeatedly.
        int[] arr = new int[7];
        int npe = 0;
        long lens = 0;
        for (int i = 0; i < 200_000; i++) {
            int[] a = (i % 997 == 0) ? null : arr;
            try {
                lens += len(a);
            } catch (NullPointerException e) {
                npe++;
            }
        }
        Fz.out("npe", npe);
        int unboxNpe = 0;
        for (int i = 0; i < 100_000; i++) {
            try {
                lens += unbox(i % 501 == 0 ? null : Integer.valueOf(i));
            } catch (NullPointerException e) {
                unboxNpe++;
            }
        }
        Fz.out("unboxNpe", unboxNpe);
        Fz.mix(lens);
        // Uncommon trap on a never-taken branch after warm-up.
        long br = 0;
        for (int i = 0; i < 400_000; i++) br += branchy(i, i == 350_000);
        Fz.mix(br);
        Fz.done();
    }

    static long branchy(int i, boolean rare) {
        if (rare) return Long.MIN_VALUE / 3 + i;
        return i * 3L;
    }

    static final class Late extends Base { int v(int x) { return x * 5 - 2; } }
}
