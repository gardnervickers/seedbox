import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.StructLayout;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Foreign-function ABI corners: homogeneous floating-point aggregates (complex numbers) passed
 * and returned through FP registers and, once those run out, on the stack, in libm and through a
 * Java upcall called back from Java; trivial downcalls (no thread-state transition) racing a
 * collector; downcalls through a library lookup whose arena is closed (dlclose); and file
 * mappings owned by arenas, with aliases that must die with their arena.
 */
public final class FfmAbi {
    static final Linker L = Linker.nativeLinker();
    static final ValueLayout.OfDouble D = ValueLayout.JAVA_DOUBLE;
    static final ValueLayout.OfFloat F = ValueLayout.JAVA_FLOAT;
    static final StructLayout C2 = MemoryLayout.structLayout(D.withName("re"), D.withName("im"));
    static final StructLayout CF = MemoryLayout.structLayout(F.withName("re"), F.withName("im"));
    static final StructLayout F4 = MemoryLayout.structLayout(F, F, F, F);
    static final StructLayout D3 = MemoryLayout.structLayout(D, D, D);
    static final StructLayout MIX = MemoryLayout.structLayout(D, ValueLayout.JAVA_LONG);
    static final StructLayout BIG = MemoryLayout.structLayout(D, D, D, D, D);
    static final MemorySegment RET = Arena.global().allocate(D3);
    static int upcalls;

    static MethodHandle fn(SymbolLookup lookup, String name, FunctionDescriptor fd, Linker.Option... opts) {
        return L.downcallHandle(lookup.find(name).orElseThrow(), fd, opts);
    }

    /** The aggregate arithmetic both the upcall and the direct Java check perform. */
    static double[] combine(double[] c2, float[] f4, int k, double[] a, double[] b, double tail, double[] c,
                            double mixD, long mixL, double[] big, float ftail) {
        double x = c2[0] * k + f4[0] + a[0] - b[2] + c[1] * tail + big[0] + mixD;
        double y = c2[1] - f4[1] * f4[2] + a[1] * b[1] + c[2] + big[2] * big[4] + (mixL ^ k);
        double z = f4[3] + a[2] + b[0] / (1 + Math.abs(tail)) + c[0] + big[1] - big[3] + ftail;
        return new double[] {x, y, z};
    }

    static double[] ds(MemorySegment s, int n) {
        double[] r = new double[n];
        for (int i = 0; i < n; i++) r[i] = s.reinterpret(n * 8L).getAtIndex(D, i);
        return r;
    }

    static float[] fs(MemorySegment s, int n) {
        float[] r = new float[n];
        for (int i = 0; i < n; i++) r[i] = s.reinterpret(n * 4L).getAtIndex(F, i);
        return r;
    }

    /** Upcall target: twelve FP registers' worth of aggregates, so later ones go on the stack. */
    static MemorySegment upcall(MemorySegment c2, MemorySegment f4, int k, MemorySegment a, MemorySegment b,
                                double tail, MemorySegment c, MemorySegment mix, MemorySegment big, float ftail) {
        upcalls++;
        double[] r = combine(ds(c2, 2), fs(f4, 4), k, ds(a, 3), ds(b, 3), tail, ds(c, 3),
            mix.get(D, 0), mix.get(ValueLayout.JAVA_LONG, 8), ds(big, 5), ftail);
        for (int i = 0; i < 3; i++) RET.setAtIndex(D, i, r[i]);
        return RET;
    }

    static MemorySegment fill(Arena a, StructLayout layout, double[] v) {
        MemorySegment s = a.allocate(layout);
        for (int i = 0; i < v.length; i++) s.setAtIndex(D, i, v[i]);
        return s;
    }

    static double[] rndD(int n) {
        double[] r = new double[n];
        for (int i = 0; i < n; i++) r[i] = (Fz.rnd.nextDouble() - 0.5) * 1000;
        return r;
    }

    static void hfa() throws Throwable {
        SymbolLookup std = L.defaultLookup();
        try (Arena a = Arena.ofConfined()) {
            SymbolLookup libm = SymbolLookup.libraryLookup("libm.so.6", a);
            MethodHandle cabs = fn(libm, "cabs", FunctionDescriptor.of(D, C2));
            MethodHandle cexp = fn(libm, "cexp", FunctionDescriptor.of(C2, C2));
            MethodHandle cpow = fn(libm, "cpow", FunctionDescriptor.of(C2, C2, C2));
            MethodHandle csqrt = fn(libm, "csqrt", FunctionDescriptor.of(C2, C2));
            MethodHandle cabsf = fn(libm, "cabsf", FunctionDescriptor.of(F, CF));
            MethodHandle cexpf = fn(libm, "cexpf", FunctionDescriptor.of(CF, CF));
            MemorySegment z = fill(a, C2, new double[] {3, 4});
            Fz.out("cabs34", (double) cabs.invokeExact(z));
            Fz.expect((double) cabs.invokeExact(z) == 5.0, "cabs(3+4i)");
            int n = 1_000 * Fz.scale;
            for (int i = 0; i < n; i++) {
                MemorySegment u = fill(a, C2, new double[] {Fz.rnd.nextDouble() * 4 - 2, Fz.rnd.nextDouble() * 6 - 3});
                MemorySegment w = fill(a, C2, new double[] {Fz.rnd.nextDouble() * 2, Fz.rnd.nextDouble() - 0.5});
                Fz.mix((double) cabs.invokeExact(u));
                MemorySegment e = (MemorySegment) cexp.invokeExact((SegmentAllocator) a, u);
                Fz.mix(e.get(D, 0));
                Fz.mix(e.get(D, 8));
                MemorySegment p = (MemorySegment) cpow.invokeExact((SegmentAllocator) a, u, w);
                Fz.mix(p.get(D, 0));
                Fz.mix(p.get(D, 8));
                MemorySegment s = (MemorySegment) csqrt.invokeExact((SegmentAllocator) a, u);
                double back = s.get(D, 0) * s.get(D, 0) - s.get(D, 8) * s.get(D, 8);
                Fz.expect(Math.abs(back - u.get(D, 0)) < 1e-9, "csqrt squared");
                MemorySegment uf = a.allocate(CF);
                uf.set(F, 0, (float) u.get(D, 0));
                uf.set(F, 4, (float) u.get(D, 8));
                Fz.mix((double) (float) cabsf.invokeExact(uf));
                MemorySegment ef = (MemorySegment) cexpf.invokeExact((SegmentAllocator) a, uf);
                Fz.mix((double) ef.get(F, 0));
                Fz.mix((double) ef.get(F, 4));
            }
            Fz.outHex("libmComplex", Fz.hash());

            FunctionDescriptor fd = FunctionDescriptor.of(D3, C2, F4, ValueLayout.JAVA_INT, D3, D3, D, D3, MIX, BIG, F);
            MethodHandle target = MethodHandles.lookup().findStatic(FfmAbi.class, "upcall", fd.toMethodType());
            MemorySegment stub = L.upcallStub(target, fd, a);
            MethodHandle down = L.downcallHandle(stub, fd);
            long agree = 0;
            for (int i = 0; i < 2_000 * Fz.scale; i++) {
                double[] c2 = rndD(2), da = rndD(3), db = rndD(3), dc = rndD(3), big = rndD(5);
                float[] f4 = {(float) Fz.rnd.nextDouble(), (float) -Fz.rnd.nextDouble(), Fz.rnd.nextInt(100), 0.25f};
                int k = Fz.rnd.nextInt(-50, 50);
                double tail = Fz.edgeDouble(), mixD = Fz.rnd.nextDouble();
                long mixL = Fz.rnd.nextLong(1 << 20);
                float ftail = (float) Fz.rnd.nextGaussian();
                MemorySegment sf4 = a.allocate(F4);
                for (int j = 0; j < 4; j++) sf4.setAtIndex(F, j, f4[j]);
                MemorySegment smix = a.allocate(MIX);
                smix.set(D, 0, mixD);
                smix.set(ValueLayout.JAVA_LONG, 8, mixL);
                MemorySegment r = (MemorySegment) down.invokeExact((SegmentAllocator) a, fill(a, C2, c2), sf4, k,
                    fill(a, D3, da), fill(a, D3, db), tail, fill(a, D3, dc), smix, fill(a, BIG, big), ftail);
                double[] want = combine(c2, f4, k, da, db, tail, dc, mixD, mixL, big, ftail);
                boolean same = true;
                for (int j = 0; j < 3; j++) {
                    same &= Double.doubleToLongBits(r.getAtIndex(D, j)) == Double.doubleToLongBits(want[j]);
                    Fz.mix(r.getAtIndex(D, j));
                }
                if (same) agree++;
            }
            Fz.out("upcallAggregates", agree + "/" + upcalls);
            Fz.expect(agree == upcalls && upcalls == 2_000 * Fz.scale, "aggregate upcall round trip");
            Fz.out("stdlibStillThere", std.find("abs").isPresent());
        }
    }

    static void trivial() throws Throwable {
        SymbolLookup std = L.defaultLookup();
        MethodHandle abs = fn(std, "abs", FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), Linker.Option.isTrivial());
        MethodHandle labs = fn(std, "labs", FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG), Linker.Option.isTrivial());
        MethodHandle absSlow = fn(std, "abs", FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        Thread gc = new Thread(() -> {
            for (int i = 0; i < 10; i++) System.gc();
        }, "fz-ffm-gc");
        gc.start();
        long sum = 0, want = 0;
        int n = 500_000 * Fz.scale;
        int base = Fz.rnd.nextInt(1 << 20);
        for (int i = 0; i < n; i++) {
            int v = (i & 1) == 0 ? -(base + (i & 1023)) : base + (i & 1023);
            sum += (int) abs.invokeExact(v);
            want += Math.abs(v);
            if ((i & 4095) == 0) {
                sum += (long) labs.invokeExact((long) -i << 20);
                want += (long) i << 20;
                sum += (int) absSlow.invokeExact(-i);
                want += i;
            }
        }
        gc.join();
        Fz.out("trivialSum", sum);
        Fz.expect(sum == want, "trivial abs sum");
        Fz.mix(sum);
    }

    static void closedLookups() throws Throwable {
        MethodHandle cached = null;
        int rejected = 0;
        double total = 0;
        for (int r = 0; r < 20; r++) {
            Arena a = Arena.ofConfined();
            SymbolLookup libm = SymbolLookup.libraryLookup("libm.so.6", a);
            MethodHandle ceil = fn(libm, "ceil", FunctionDescriptor.of(D, D));
            MethodHandle floor = fn(libm, "floor", FunctionDescriptor.of(D, D));
            for (int i = 0; i < 5_000; i++) {
                double x = Fz.rnd.nextDouble() * 200 - 100;
                total += (double) ceil.invokeExact(x) - (double) floor.invokeExact(x);
            }
            if (cached == null) cached = ceil;
            a.close();
            try {
                double unused = (double) ceil.invokeExact(1.25);
            } catch (IllegalStateException e) {
                rejected++;
            }
        }
        try {
            double unused = (double) cached.invokeExact(2.5);
        } catch (IllegalStateException e) {
            rejected++;
        }
        Fz.out("closedLookupRejected", rejected);
        Fz.out("ceilMinusFloor", (long) total);
        Fz.expect(rejected == 21, "closed library arena rejects calls");
        // A library the JVM does not load itself: each close really unloads it.
        String last = "absent";
        int loads = 0;
        for (int r = 0; r < 6; r++) {
            Arena a = Arena.ofConfined();
            SymbolLookup crypt;
            try {
                crypt = SymbolLookup.libraryLookup("libcrypt.so.1", a);
            } catch (IllegalArgumentException e) {
                a.close();
                break;
            }
            MethodHandle c = fn(crypt, "crypt", FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
            MemorySegment out = (MemorySegment) c.invokeExact(a.allocateUtf8String("key" + (Fz.seed + r)),
                a.allocateUtf8String("$6$fzsalt" + r + "$"));
            last = out.reinterpret(256).getUtf8String(0);
            Fz.mix(last);
            loads++;
            a.close();
            try {
                MemorySegment unused = (MemorySegment) c.invokeExact(MemorySegment.NULL, MemorySegment.NULL);
                Fz.expect(false, "crypt after close");
            } catch (IllegalStateException e) {
                Fz.mix(r);
            }
        }
        Fz.out("cryptLoads", loads);
        Fz.out("cryptLast", last);
    }

    static void mappings() throws Exception {
        Path path = Files.createTempFile("fzmap", ".bin");
        int closedSeen = 0, aliasAgree = 0, readOnlyRefused = 0, privateKept = 0;
        long fileSum = 0;
        for (int r = 0; r < 20 * Fz.scale; r++) {
            int size = 4096 * (1 + Fz.rnd.nextInt(4)) + Fz.rnd.nextInt(4096);
            Files.write(path, new byte[size]);
            int[] vals = new int[size / 4];
            for (int i = 0; i < vals.length; i++) vals[i] = Fz.rnd.nextInt();
            try (FileChannel ch = FileChannel.open(path, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                Arena aa = Arena.ofConfined(), ab = Arena.ofConfined();
                MemorySegment ma = ch.map(FileChannel.MapMode.READ_WRITE, 0, size, aa);
                MemorySegment mb = ch.map(FileChannel.MapMode.READ_WRITE, 0, size, ab);
                MemorySegment tail = ma.asSlice(size - (size % 4) - 4, 4);
                MemorySegment mid = ma.asSlice(4096, size - 4096);
                for (int i = 0; i < vals.length; i++) ma.setAtIndex(ValueLayout.JAVA_INT_UNALIGNED, i, vals[i]);
                boolean agree = true;
                for (int i = 0; i < vals.length; i++) agree &= mb.getAtIndex(ValueLayout.JAVA_INT_UNALIGNED, i) == vals[i];
                if (agree) aliasAgree++;
                tail.set(ValueLayout.JAVA_INT_UNALIGNED, 0, r * 31 + 7);
                vals[vals.length - 1] = r * 31 + 7;
                ma.force();
                aa.close();
                for (MemorySegment dead : new MemorySegment[] {ma, tail, mid}) {
                    try {
                        dead.get(ValueLayout.JAVA_BYTE, 0);
                    } catch (IllegalStateException e) {
                        closedSeen++;
                    }
                }
                Fz.expect(mb.getAtIndex(ValueLayout.JAVA_INT_UNALIGNED, vals.length - 1) == r * 31 + 7, "alias outlives other arena");
                mb.setAtIndex(ValueLayout.JAVA_INT_UNALIGNED, 0, ~vals[0]);
                vals[0] = ~vals[0];
                ab.close();
                try (Arena ar = Arena.ofConfined()) {
                    MemorySegment ro = ch.map(FileChannel.MapMode.READ_ONLY, 0, size, ar);
                    try {
                        ro.set(ValueLayout.JAVA_BYTE, 0, (byte) 1);
                    } catch (UnsupportedOperationException e) {
                        readOnlyRefused++;
                    }
                    MemorySegment priv = ch.map(FileChannel.MapMode.PRIVATE, 0, size, ar);
                    priv.setAtIndex(ValueLayout.JAVA_INT_UNALIGNED, 0, 12345);
                    if (ro.getAtIndex(ValueLayout.JAVA_INT_UNALIGNED, 0) == vals[0]) privateKept++;
                }
            }
            ByteBuffer file = ByteBuffer.wrap(Files.readAllBytes(path)).order(ByteOrder.nativeOrder());
            for (int i = 0; i < vals.length; i++) {
                Fz.expect(file.getInt(i * 4) == vals[i], "file contents after unmap");
                fileSum += vals[i];
            }
        }
        Files.delete(path);
        Fz.out("mappingAliases", aliasAgree + " closed=" + closedSeen + " ro=" + readOnlyRefused + " private=" + privateKept);
        Fz.out("mappedFileSum", fileSum);
        Fz.expect(closedSeen == 60 * Fz.scale, "every alias dies with its arena");
        Fz.mix(fileSum);
    }

    public static void main(String[] args) throws Throwable {
        Fz.init("FfmAbi", args);
        hfa();
        trivial();
        closedLookups();
        mappings();
        Fz.done();
    }
}
