import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Class resolution and initialization states the VM owns: CONSTANT_Dynamic constants resolved
 * through ldc/ldc2_w in a hand-assembled hidden class (JDK bootstraps, nested dynamic arguments,
 * a racing resolution and a bootstrap failure that stays recorded), hidden-class data reached
 * through a dynamic constant and through MethodHandles.classData across collections, classes left
 * permanently erroneous by a failed initializer, initializers that park while another thread
 * waits on the VM's initialization lock, and circular initialization in one thread.
 */
public final class ClassInit {
    // ---------------------------------------------------------------- dynamic-constant bootstraps

    static final AtomicInteger objectCalls = new AtomicInteger(), badCalls = new AtomicInteger();

    static Object bsmObject(MethodHandles.Lookup l, String name, Class<?> type) {
        objectCalls.incrementAndGet();
        return new StringBuilder(name);
    }

    static Object bsmBad(MethodHandles.Lookup l, String name, Class<?> type) {
        badCalls.incrementAndGet();
        throw new IllegalStateException("bootstrap " + name);
    }

    static Object bsmChain(MethodHandles.Lookup l, String name, Class<?> type, Object prev) {
        return (Long) prev * 3 + 1;
    }

    /** A minimal class-file writer: constant pool, static methods and BootstrapMethods. */
    static final class ClassFile {
        final ByteArrayOutputStream pool = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(pool);
        final Map<String, Integer> index = new HashMap<>();
        final List<byte[]> methods = new ArrayList<>();
        final List<int[]> bootstraps = new ArrayList<>();
        int next = 1;

        int entry(String key, int slots, IoWriter w) {
            Integer i = index.get(key);
            if (i != null) return i;
            try {
                w.write(cp);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            int at = next;
            next += slots;
            index.put(key, at);
            return at;
        }

        interface IoWriter { void write(DataOutputStream o) throws IOException; }

        int utf8(String s) { return entry("U" + s, 1, o -> { o.writeByte(1); o.writeUTF(s); }); }
        int cls(String n) { int u = utf8(n); return entry("C" + n, 1, o -> { o.writeByte(7); o.writeShort(u); }); }
        int integer(int v) { return entry("I" + v, 1, o -> { o.writeByte(3); o.writeInt(v); }); }
        int lng(long v) { return entry("J" + v, 2, o -> { o.writeByte(5); o.writeLong(v); }); }
        int nat(String n, String d) {
            int a = utf8(n), b = utf8(d);
            return entry("N" + n + ":" + d, 1, o -> { o.writeByte(12); o.writeShort(a); o.writeShort(b); });
        }
        int methodref(String c, String n, String d) {
            int a = cls(c), b = nat(n, d);
            return entry("M" + c + "." + n + d, 1, o -> { o.writeByte(10); o.writeShort(a); o.writeShort(b); });
        }
        int staticHandle(String c, String n, String d) {
            int r = methodref(c, n, d);
            return entry("H" + r, 1, o -> { o.writeByte(15); o.writeByte(6); o.writeShort(r); });
        }
        int dynamic(String name, String type, int bsm, int... args) {
            int[] b = new int[args.length + 1];
            b[0] = bsm;
            System.arraycopy(args, 0, b, 1, args.length);
            int bi = bootstraps.size();
            bootstraps.add(b);
            int t = nat(name, type);
            return entry("D" + bi, 1, o -> { o.writeByte(17); o.writeShort(bi); o.writeShort(t); });
        }

        /** A public static method that loads one constant and returns it. */
        void constantMethod(String name, String desc, int constant, boolean wide) {
            int n = utf8(name), d = utf8(desc), code = utf8("Code");
            char ret = desc.charAt(desc.length() - 1);
            boolean twoSlot = desc.endsWith(")J") || desc.endsWith(")D");
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            if (twoSlot) { b.write(0x14); b.write(constant >> 8); b.write(constant); }
            else if (!wide && constant < 256) { b.write(0x12); b.write(constant); }
            else { b.write(0x13); b.write(constant >> 8); b.write(constant); }
            b.write(switch (ret) { case 'I' -> 0xac; case 'J' -> 0xad; default -> 0xb0; });
            byte[] insns = b.toByteArray();
            ByteArrayOutputStream m = new ByteArrayOutputStream();
            try (DataOutputStream o = new DataOutputStream(m)) {
                o.writeShort(0x0009);
                o.writeShort(n);
                o.writeShort(d);
                o.writeShort(1);
                o.writeShort(code);
                o.writeInt(12 + insns.length);
                o.writeShort(2);
                o.writeShort(0);
                o.writeInt(insns.length);
                o.write(insns);
                o.writeShort(0);
                o.writeShort(0);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            methods.add(m.toByteArray());
        }

        byte[] bytes(String name) throws IOException {
            int self = cls(name), sup = cls("java/lang/Object"), bsmAttr = utf8("BootstrapMethods");
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            DataOutputStream o = new DataOutputStream(out);
            o.writeInt(0xCAFEBABE);
            o.writeShort(0);
            o.writeShort(61);
            o.writeShort(next);
            cp.flush();
            o.write(pool.toByteArray());
            o.writeShort(0x0031);
            o.writeShort(self);
            o.writeShort(sup);
            o.writeShort(0);
            o.writeShort(0);
            o.writeShort(methods.size());
            for (byte[] m : methods) o.write(m);
            o.writeShort(1);
            o.writeShort(bsmAttr);
            int len = 2;
            for (int[] b : bootstraps) len += 4 + 2 * (b.length - 1);
            o.writeInt(len);
            o.writeShort(bootstraps.size());
            for (int[] b : bootstraps) {
                o.writeShort(b[0]);
                o.writeShort(b.length - 1);
                for (int i = 1; i < b.length; i++) o.writeShort(b[i]);
            }
            o.flush();
            return out.toByteArray();
        }
    }

    static final String LOOKUP = "Ljava/lang/invoke/MethodHandles$Lookup;";
    static final String BSM3 = "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;";

    static byte[] condyClass(int iv, long jv, int a, int b) throws IOException {
        ClassFile c = new ClassFile();
        String cb = "java/lang/invoke/ConstantBootstraps";
        int cast = c.staticHandle(cb, "explicitCast", "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/Object;");
        int ci = c.dynamic("i", "I", cast, c.integer(iv));
        int cj = c.dynamic("j", "J", cast, c.lng(jv));
        int chain = c.dynamic("chain", "J", c.staticHandle("ClassInit", "bsmChain",
            "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/Object;"), cj);
        int obj = c.dynamic("obj", "Ljava/lang/Object;", c.staticHandle("ClassInit", "bsmObject", BSM3));
        int bad = c.dynamic("bad", "Ljava/lang/Object;", c.staticHandle("ClassInit", "bsmBad", BSM3));
        String dataAt = "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;I)Ljava/lang/Object;";
        int data = c.dynamic("_", "[B", c.staticHandle("java/lang/invoke/MethodHandles", "classDataAt", dataAt), c.integer(0));
        int label = c.dynamic("_", "Ljava/lang/String;", c.staticHandle("java/lang/invoke/MethodHandles", "classDataAt", dataAt), c.integer(1));
        int all = c.dynamic("_", "Ljava/util/List;", c.staticHandle("java/lang/invoke/MethodHandles", "classData", BSM3));
        int nul = c.dynamic("n", "Ljava/lang/String;", c.staticHandle(cb, "nullConstant", BSM3));
        int prim = c.dynamic("J", "Ljava/lang/Class;", c.staticHandle(cb, "primitiveClass",
            "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Class;"));
        int max = c.staticHandle("java/lang/Math", "max", "(II)I");
        int inv = c.dynamic("max", "I", c.staticHandle(cb, "invoke",
            "(" + LOOKUP + "Ljava/lang/String;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;"),
            max, c.integer(a), c.integer(b));
        c.constantMethod("i", "()I", ci, false);
        c.constantMethod("iw", "()I", ci, true);
        c.constantMethod("j", "()J", cj, false);
        c.constantMethod("chain", "()J", chain, false);
        c.constantMethod("obj", "()Ljava/lang/Object;", obj, true);
        c.constantMethod("bad", "()Ljava/lang/Object;", bad, true);
        c.constantMethod("data", "()[B", data, true);
        c.constantMethod("label", "()Ljava/lang/String;", label, true);
        c.constantMethod("all", "()Ljava/util/List;", all, true);
        c.constantMethod("nul", "()Ljava/lang/String;", nul, true);
        c.constantMethod("prim", "()Ljava/lang/Class;", prim, true);
        c.constantMethod("max", "()I", inv, true);
        return c.bytes("ClassInitCondy");
    }

    static MethodHandle find(MethodHandles.Lookup l, String name, Class<?> ret) throws Exception {
        return l.findStatic(l.lookupClass(), name, MethodType.methodType(ret));
    }

    static void condy() throws Throwable {
        WeakReference<?>[] w = condyBody();
        // Without the class nothing roots its data: it is collected with the class.
        boolean gone = false;
        for (int k = 0; k < 20 && !gone; k++) {
            System.gc();
            gone = w[0].get() == null && w[1].get() == null;
        }
        Fz.out("classData.unloaded", gone);
    }

    static WeakReference<?>[] condyBody() throws Throwable {
        int iv = Fz.edgeInt(), a = Fz.rnd.nextInt(-1000, 1000), b = Fz.rnd.nextInt(-1000, 1000);
        long jv = Fz.edgeLong();
        byte[] payload = Fz.bytes(64 + Fz.rnd.nextInt(4096));
        String text = "label" + Fz.rnd.nextInt(1000);
        byte[] bytes = condyClass(iv, jv, a, b);
        MethodHandles.Lookup l = MethodHandles.lookup().defineHiddenClassWithClassData(
            bytes, List.of(payload, text, 7), false);
        MethodHandle i = find(l, "i", int.class), iw = find(l, "iw", int.class), j = find(l, "j", long.class);
        MethodHandle chain = find(l, "chain", long.class), obj = find(l, "obj", Object.class);
        MethodHandle bad = find(l, "bad", Object.class), data = find(l, "data", byte[].class);
        MethodHandle label = find(l, "label", String.class), all = find(l, "all", List.class);
        MethodHandle nul = find(l, "nul", String.class), prim = find(l, "prim", Class.class);
        MethodHandle max = find(l, "max", int.class);

        // Several threads resolve the same object constant at once; exactly one value wins.
        Object[] seen = new Object[4];
        CountDownLatch start = new CountDownLatch(1);
        Thread[] ts = new Thread[seen.length];
        for (int t = 0; t < ts.length; t++) {
            int k = t;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    seen[k] = (Object) obj.invokeExact();
                } catch (Throwable e) {
                    seen[k] = e;
                }
            });
            ts[t].start();
        }
        start.countDown();
        Fz.join(ts);
        boolean same = true;
        for (Object o : seen) same &= o == seen[0] && o instanceof StringBuilder;
        Fz.out("condy.raceOneValue", same);
        Fz.expect(objectCalls.get() >= 1, "object bootstrap ran");

        long sum = 0, x = 0;
        int n = 100_000 * Fz.scale;
        for (int k = 0; k < n; k++) {
            sum += (int) i.invokeExact() + (int) iw.invokeExact();
            x = x * 31 + (long) j.invokeExact() + (long) chain.invokeExact() + (int) max.invokeExact();
            if ((Object) obj.invokeExact() != seen[0]) Fz.expect(false, "object constant changed");
        }
        Fz.out("condy.int", iv);
        Fz.out("condy.long", jv);
        Fz.out("condy.chain", (long) chain.invokeExact());
        Fz.out("condy.invokeMax", (int) max.invokeExact());
        Fz.out("condy.loop", sum + " " + x);
        Fz.expect((long) chain.invokeExact() == jv * 3 + 1, "nested dynamic argument");
        Fz.expect((int) max.invokeExact() == Math.max(a, b), "ConstantBootstraps.invoke");
        Fz.mix(sum);
        Fz.mix(x);
        Fz.out("condy.null", (String) nul.invokeExact() == null);
        Fz.out("condy.primitive", ((Class<?>) prim.invokeExact()).getName());

        // A failing bootstrap: the error is recorded, and the bootstrap never runs again.
        List<String> errors = new ArrayList<>();
        for (int k = 0; k < 3; k++) {
            try {
                Object unused = (Object) bad.invokeExact();
                errors.add("none");
            } catch (BootstrapMethodError e) {
                errors.add(e.getClass().getSimpleName() + "/" + (e.getCause() == null ? "-" : e.getCause().getClass().getSimpleName()));
            }
        }
        Fz.out("condy.badErrors", errors);
        Fz.out("condy.badCalls", badCalls.get());

        // Class data as a dynamic constant, across collections that move it.
        WeakReference<byte[]> weakData = new WeakReference<>(payload);
        payload = null;
        for (int k = 0; k < 3; k++) System.gc();
        byte[] got = (byte[]) data.invokeExact();
        Fz.out("classData.condyHash", java.util.Arrays.hashCode(got));
        Fz.out("classData.label", (String) label.invokeExact());
        Fz.out("classData.listSize", ((List<?>) all.invokeExact()).size());
        Fz.out("classData.rooted", weakData.get() == got);
        Fz.mix(got);
        return new WeakReference<?>[] {new WeakReference<>(l.lookupClass()), weakData};
    }

    /** The javac-compiled form: MethodHandles.classData as a call. */
    static void classDataCall() throws Throwable {
        byte[] bytes;
        try (InputStream in = ClassInit.class.getResourceAsStream("/ClassInitHidden.class")) {
            bytes = in.readAllBytes();
        }
        int[] data = new int[256 + Fz.rnd.nextInt(256)];
        for (int k = 0; k < data.length; k++) data[k] = Fz.rnd.nextInt();
        long expect = 0;
        for (int v : data) expect = expect * 31 + v;
        List<MethodHandle> hs = new ArrayList<>();
        for (int k = 0; k < 8; k++) {
            int[] copy = data.clone();
            copy[0] += k;
            MethodHandles.Lookup l = MethodHandles.lookup().defineHiddenClassWithClassData(bytes, copy, k % 2 == 0);
            hs.add(l.findStatic(l.lookupClass(), "f", MethodType.methodType(long.class)));
        }
        data = null;
        System.gc();
        long sum = 0;
        for (int r = 0; r < 2000 * Fz.scale; r++)
            for (MethodHandle h : hs) sum += (long) h.invokeExact();
        long one = (long) hs.get(0).invokeExact();
        Fz.out("classData.callMatches", one == expect);
        Fz.out("classData.callSum", sum);
        Fz.mix(sum);
    }

    // ---------------------------------------------------------------- erroneous classes

    static int failValue;

    static int fail() {
        if (failValue >= 0) throw new IllegalStateException("x");
        return failValue;
    }

    static class Bad {
        static final int X = fail();
        static int get() { return X; }
    }

    static class BadError {
        static final int X;
        static {
            if (failValue >= 0) throw new AssertionError("raw error escapes unwrapped");
            X = 1;
        }
    }

    static String describe(Throwable t) {
        StringBuilder b = new StringBuilder(t.getClass().getSimpleName());
        for (Throwable c = t.getCause(); c != null; c = c.getCause()) b.append('<').append(c.getClass().getSimpleName());
        return b.toString();
    }

    static void erroneous() throws Exception {
        failValue = Fz.rnd.nextInt(1000);
        List<String> seen = new ArrayList<>();
        for (int k = 0; k < 2; k++) {
            try {
                seen.add("value" + Bad.X);
            } catch (LinkageError e) {
                seen.add(describe(e));
            }
        }
        try {
            seen.add("value" + Bad.get());
        } catch (LinkageError e) {
            seen.add(describe(e));
        }
        try {
            Class.forName("ClassInit$Bad");
        } catch (LinkageError e) {
            seen.add("forName:" + e.getClass().getSimpleName());
        }
        try {
            MethodHandle h = MethodHandles.lookup().findStatic(Bad.class, "get", MethodType.methodType(int.class));
            seen.add("handle" + (int) h.invokeExact());
        } catch (Throwable e) {
            seen.add("handle:" + e.getClass().getSimpleName());
        }
        try {
            Bad.class.getDeclaredMethod("get").invoke(null);
        } catch (java.lang.reflect.InvocationTargetException e) {
            seen.add("reflect:" + describe(e.getCause()));
        } catch (LinkageError e) {
            seen.add("reflect:" + e.getClass().getSimpleName());
        }
        String[] other = new String[1];
        Thread t = new Thread(() -> {
            try {
                other[0] = "value" + Bad.X;
            } catch (LinkageError e) {
                other[0] = e.getClass().getSimpleName();
            }
        });
        t.start();
        t.join();
        seen.add("thread:" + other[0]);
        for (int k = 0; k < 2; k++) {
            try {
                seen.add("value" + BadError.X);
            } catch (Error e) {
                seen.add(describe(e));
            }
        }
        Fz.out("erroneous", seen);
        Fz.mix(seen.toString());
    }

    // ---------------------------------------------------------------- parked initializers

    static final CountDownLatch slowReady = new CountDownLatch(1), slowGo = new CountDownLatch(1);
    static final CountDownLatch badReady = new CountDownLatch(1), badGo = new CountDownLatch(1);
    static int slowValue;

    static int park(CountDownLatch ready, CountDownLatch go, boolean fail) {
        ready.countDown();
        try {
            go.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
        if (fail) throw new IllegalStateException("parked failure");
        return slowValue;
    }

    static class Slow { static final int X = park(slowReady, slowGo, false); }

    static class SlowBad { static final int X = park(badReady, badGo, true); }

    /**
     * HotSpot reports a thread waiting for another thread's initializer as RUNNABLE, so wait
     * until the thread has started its task and give it time to reach the initialization lock.
     * Whether it is already waiting when the initializer finishes does not change any result.
     */
    static void awaitWaiting(Thread t) throws InterruptedException {
        for (int k = 0; k < 2_000 && t.getState() == Thread.State.NEW; k++) Thread.sleep(1);
        Thread.sleep(20);
    }

    static void parked() throws Exception {
        slowValue = 1 + Fz.rnd.nextInt(1 << 20);
        int[] v = new int[2];
        boolean[] interrupted = new boolean[1];
        Thread a = new Thread(() -> v[0] = Slow.X), b = new Thread(() -> {
            v[1] = Slow.X;
            interrupted[0] = Thread.currentThread().isInterrupted();
        });
        a.start();
        slowReady.await();
        b.start();
        awaitWaiting(b);
        // An interrupt does not end a wait for class initialization; the flag stays set.
        b.interrupt();
        Thread.sleep(2);
        boolean stillWaiting = b.isAlive();
        slowGo.countDown();
        a.join();
        b.join();
        Fz.out("parked.interruptIgnored", stillWaiting && interrupted[0]);
        Fz.out("parked.values", (v[0] == slowValue) + " " + (v[1] == slowValue));
        Fz.mix(v[0] + v[1]);

        String[] r = new String[2];
        Thread c = new Thread(() -> {
            try {
                r[0] = "value" + SlowBad.X;
            } catch (LinkageError e) {
                r[0] = describe(e);
            }
        }), d = new Thread(() -> {
            try {
                r[1] = "value" + SlowBad.X;
            } catch (LinkageError e) {
                r[1] = e.getClass().getSimpleName();
            }
        });
        c.start();
        badReady.await();
        d.start();
        awaitWaiting(d);
        badGo.countDown();
        c.join();
        d.join();
        Fz.out("parkedFailure", r[0] + " " + r[1]);
        Fz.mix(r[0] + r[1]);
    }

    // ---------------------------------------------------------------- circular initialization

    static class CycleA { static final int A; static { A = CycleB.B + 1; } static int peek() { return A; } }
    static class CycleB { static final int B; static { B = CycleA.peek() + 10; } }

    public static void main(String[] args) throws Throwable {
        Fz.init("ClassInit", args);
        condy();
        classDataCall();
        erroneous();
        parked();
        // CycleB sees CycleA's default value during CycleA's own initialization.
        Fz.out("cycle", CycleA.A + " " + CycleB.B);
        Fz.mix(CycleA.A * 100 + CycleB.B);
        Fz.done();
    }
}

/** Hidden-class body for ClassInit: sums its int[] class data through MethodHandles.classData. */
final class ClassInitHidden {
    static long f() throws IllegalAccessException {
        int[] d = MethodHandles.classData(MethodHandles.lookup(), "_", int[].class);
        long h = 0;
        for (int v : d) h = h * 31 + v;
        return h;
    }
}
