import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Method-handle adapters at the 255-slot argument limit (collectors, permutations, filters and
 * spreaders over 127 longs or 254 ints, and the errors one slot further), plus the cleanup and
 * loop combinators: tryFinally preserving a throwable or return value, cleanups that replace
 * the exception, catchException, and counted/iterated/while loops. Each adapter runs long
 * enough to be compiled.
 */
public final class MhAdapters {
    static final MethodHandles.Lookup L = MethodHandles.lookup();
    static int cleanups;

    static long weighted(long[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) s += a[i] * (i + 1);
        return s;
    }

    static int intSum(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s = s * 31 + a[i];
        return s;
    }

    static long scramble(long x) { return Long.rotateLeft(x, 7) ^ 0x5555_5555_5555_5555L; }

    static MethodHandle mh(String name, Class<?> ret, Class<?>... params) throws Exception {
        return L.findStatic(MhAdapters.class, name, MethodType.methodType(ret, params));
    }

    static String error(ThrowingRunnable r) {
        try {
            r.run();
            return "none";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }

    interface ThrowingRunnable { void run() throws Throwable; }

    static void wide() throws Throwable {
        int n = 127;
        int[] perm = new int[n];
        List<Integer> order = new ArrayList<>();
        for (int i = 0; i < n; i++) order.add(i);
        Collections.shuffle(order, new java.util.Random(Fz.seed));
        for (int i = 0; i < n; i++) perm[i] = order.get(i);
        MethodHandle collector = mh("weighted", long.class, long[].class).asCollector(long[].class, n);
        MethodHandle permuted = MethodHandles.permuteArguments(collector, collector.type(), perm);
        int filtered = Fz.rnd.nextInt(n);
        MethodHandle[] filters = new MethodHandle[n];
        filters[filtered] = mh("scramble", long.class, long.class);
        MethodHandle chain = MethodHandles.filterArguments(permuted, 0, filters);
        MethodHandle spread = chain.asSpreader(long[].class, n);
        long[] args = new long[n];
        long sum = 0;
        int iters = 30_000 * Fz.scale;
        for (int k = 0; k < iters; k++) {
            args[k % n] = Fz.edgeLong();
            sum = sum * 31 + (long) spread.invokeExact(args);
        }
        long[] filteredArgs = args.clone();
        filteredArgs[filtered] = scramble(args[filtered]);
        long[] reordered = new long[n];
        for (int i = 0; i < n; i++) reordered[i] = filteredArgs[perm[i]];
        long expect = weighted(reordered);
        long last = (long) spread.invokeExact(args);
        Object[] boxed = new Object[n];
        for (int i = 0; i < n; i++) boxed[i] = args[i];
        long generic = (long) chain.invokeWithArguments(boxed);
        Fz.out("wide.longSlots", chain.type().parameterCount() * 2);
        Fz.out("wide.matches", (last == expect) + " " + (generic == expect));
        Fz.out("wide.sum", sum);
        Fz.mix(sum);

        // 254 int slots: dropArguments around a constant and a collector with inserted arguments.
        MethodHandle ints = mh("intSum", int.class, int[].class).asCollector(int[].class, 254);
        MethodHandle bound = MethodHandles.insertArguments(ints, 0, 7, 11);
        MethodHandle widened = MethodHandles.dropArguments(bound, 252, int.class, int.class);
        MethodHandle ispread = widened.asSpreader(int[].class, 254);
        int[] iargs = new int[254];
        int isum = 0;
        for (int k = 0; k < iters; k++) {
            iargs[(k * 13) % 254] = Fz.edgeInt();
            isum = isum * 17 + (int) ispread.invokeExact(iargs);
        }
        int[] iexpect = new int[254];
        iexpect[0] = 7;
        iexpect[1] = 11;
        System.arraycopy(iargs, 0, iexpect, 2, 252);
        Fz.out("wide.intMatches", (int) ispread.invokeExact(iargs) == intSum(iexpect));
        Fz.out("wide.intSum", isum);
        Fz.mix(isum);

        // One slot beyond the limit is rejected when the adapter is made, never at the call.
        List<String> limits = new ArrayList<>();
        limits.add("collect128:" + error(() -> mh("weighted", long.class, long[].class).asCollector(long[].class, 128)));
        limits.add("type255ints:" + error(() -> MethodType.methodType(int.class, Collections.nCopies(255, int.class))));
        limits.add("type254ints:" + error(() -> MethodType.methodType(int.class, Collections.nCopies(254, int.class))));
        limits.add("drop255:" + error(() -> MethodHandles.dropArguments(MethodHandles.constant(int.class, 1), 0,
            Collections.nCopies(255, int.class))));
        limits.add("invoker127longs:" + error(() -> MethodHandles.invoker(collector.type())));
        limits.add("invoker126longs:" + error(() -> MethodHandles.invoker(
            MethodType.methodType(long.class, Collections.nCopies(126, long.class)))));
        Fz.out("wide.limits", limits);
        Fz.mix(limits.toString());
    }

    static int target(int x) {
        if (x < 0) throw new IllegalArgumentException("negative");
        if (x == 0) throw new ArithmeticException("zero");
        return x * 3;
    }

    static int cleanup(Throwable t, int value, int x) {
        cleanups++;
        return t == null ? value + 1 : -1;
    }

    static int replacing(Throwable t, int value, int x) {
        cleanups++;
        if (t instanceof ArithmeticException) throw new UnsupportedOperationException("replaced");
        return value;
    }

    static int voidCount;

    static void effect(int x) {
        if (x % 5 == 0) throw new IllegalStateException();
        voidCount += x;
    }

    static void voidCleanup(Throwable t, int x) { cleanups++; }

    static int negate(IllegalArgumentException e, int x) { return -x * 7; }

    static int step(int acc, int i) { return acc * 31 + i; }

    static void combinators() throws Throwable {
        MethodHandle t = mh("target", int.class, int.class);
        MethodHandle fin = MethodHandles.tryFinally(t, mh("cleanup", int.class, Throwable.class, int.class, int.class));
        MethodHandle rep = MethodHandles.tryFinally(t, mh("replacing", int.class, Throwable.class, int.class, int.class));
        MethodHandle caught = MethodHandles.catchException(fin, IllegalArgumentException.class,
            mh("negate", int.class, IllegalArgumentException.class, int.class));
        MethodHandle vfin = MethodHandles.tryFinally(mh("effect", void.class, int.class),
            mh("voidCleanup", void.class, Throwable.class, int.class));
        long sum = 0;
        int[] errors = new int[4];
        int iters = 50_000 * Fz.scale;
        for (int k = 0; k < iters; k++) {
            int x = Fz.rnd.nextInt(-20, 100);
            try {
                sum += (int) fin.invokeExact(x);
            } catch (IllegalArgumentException e) {
                errors[0]++;
            } catch (ArithmeticException e) {
                errors[1]++;
            }
            try {
                sum += (int) rep.invokeExact(x);
            } catch (UnsupportedOperationException e) {
                errors[2]++;
            } catch (IllegalArgumentException e) {
                sum -= 1;
            }
            try {
                sum += (int) caught.invokeExact(x);
            } catch (ArithmeticException e) {
                sum -= 2;
            }
            try {
                vfin.invokeExact(x);
            } catch (IllegalStateException e) {
                errors[3]++;
            }
        }
        Fz.out("finally.sum", sum);
        Fz.out("finally.errors", java.util.Arrays.toString(errors));
        Fz.out("finally.cleanups", cleanups);
        Fz.out("finally.voidCount", voidCount);
        Fz.expect(cleanups == 4 * iters, "a cleanup for every call");
        Fz.mix(sum);
        Fz.mix(cleanups);
        Fz.mix(voidCount);

        // Loops: counted (with a step taking the counter), iterated over a list, while.
        int count = 50 + Fz.rnd.nextInt(200);
        MethodHandle counted = MethodHandles.countedLoop(MethodHandles.constant(int.class, count),
            MethodHandles.constant(int.class, 1),
            MethodHandles.permuteArguments(mh("step", int.class, int.class, int.class),
                MethodType.methodType(int.class, int.class, int.class), 0, 1));
        int loops = 0;
        for (int k = 0; k < 2000 * Fz.scale; k++) loops = loops * 7 + (int) counted.invokeExact();
        int direct = 1;
        for (int i = 0; i < count; i++) direct = direct * 31 + i;
        Fz.out("loop.counted", ((int) counted.invokeExact() == direct) + " " + loops);
        Fz.mix(loops);

        List<Integer> items = new ArrayList<>();
        for (int i = 0; i < 64; i++) items.add(Fz.rnd.nextInt(1000));
        MethodHandle body = L.findStatic(MhAdapters.class, "accumulate",
            MethodType.methodType(long.class, long.class, Integer.class));
        MethodHandle iterated = MethodHandles.iteratedLoop(null, MethodHandles.constant(long.class, 3L), body)
            .asType(MethodType.methodType(long.class, List.class));
        long it = 0;
        for (int k = 0; k < 2000 * Fz.scale; k++) it ^= (long) iterated.invokeExact((List<?>) items) + k;
        long expect = 3;
        for (int v : items) expect = accumulate(expect, v);
        Fz.out("loop.iterated", ((long) iterated.invokeExact((List<?>) items) == expect) + " " + it);
        Fz.mix(it);

        MethodHandle whileLoop = MethodHandles.whileLoop(
            mh("whileInit", long.class, long.class),
            mh("whilePred", boolean.class, long.class, long.class),
            mh("whileBody", long.class, long.class, long.class));
        long w = 0;
        for (int k = 0; k < 2000 * Fz.scale; k++) w += (long) whileLoop.invokeExact((long) k);
        Fz.out("loop.while", w);
        Fz.mix(w);
    }

    static long accumulate(long acc, Integer v) { return acc * 131 + v; }
    static long whileInit(long x) { return x; }
    static boolean whilePred(long v, long x) { return v < 1_000_000; }
    static long whileBody(long v, long x) { return v * 3 + x % 7 + 1; }

    public static void main(String[] args) throws Throwable {
        Fz.init("MhAdapters", args);
        wide();
        combinators();
        Fz.done();
    }
}
