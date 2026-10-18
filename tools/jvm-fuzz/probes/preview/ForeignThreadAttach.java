import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.concurrent.Semaphore;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Threads HotSpot did not create: short batches of pthreads from a JNI helper (libfzjni.so)
 * attach (as user or daemon threads, some detaching and attaching again), call Java with nine
 * integer and nine floating-point arguments, allocate, throw, and detach, while another thread
 * keeps compiling, dropping hidden classes and collecting. A second phase calls a
 * foreign-function upcall stub from pthreads that were never attached, so the upcall attaches
 * them and their exit detaches them. The helper checks every result against its own model;
 * pthread stacks and TLS are reused from batch to batch.
 */
public final class ForeignThreadAttach {
    static native void init();
    static native long runBatch(int batch, int threads, int calls, long seed, int mode);
    static native long mismatches();
    static native long exceptions();

    static long cb(long a, int b, double c, float d, long e, double f, int g, double h, double i8, double i9,
                   double i10, double i11, double i12, long j13, long j14, long j15, long j16, long j17) {
        long x = 0x9E3779B97F4A7C15L;
        long[] ints = {a, b, e, g, j13, j14, j15, j16, j17};
        for (int i = 0; i < ints.length; i++) x = (x ^ ints[i]) * 0x100000001B3L + i;
        double s = c * 1.5 + f - h * 0.25 + i8 + i9 * 3.0 - i10 + i11 * 0.125 + i12;
        float fl = d * 2.0f + 1.0f;
        return x ^ Double.doubleToRawLongBits(s) ^ ((Float.floatToRawIntBits(fl) & 0xffffffffL) << 7);
    }

    static volatile Object keep;

    static long nameHash(int k) {
        keep = new long[k % 64 + 1];
        Thread t = Thread.currentThread();
        return (long) t.getName().hashCode() ^ (t.isDaemon() ? 1L << 40 : 0);
    }

    static long maybeThrow(long x) {
        if ((x & 1) != 0) throw new IllegalStateException("odd " + x);
        return x;
    }

    static long strHash(String s) { return s.hashCode(); }

    static final AtomicLong daemonUpcalls = new AtomicLong();

    static long upcall(long a, int b, double c, float d, long e, double f, long g, long h, long i, long j, long k,
                       long l, double m, double n, double o, double p, double q, double r) {
        long x = 0xC2B2AE3D27D4EB4FL;
        long[] ints = {a, b, e, g, h, i, j, k, l, 7};
        for (int z = 0; z < ints.length; z++) x = (x ^ ints[z]) * 0x9E3779B97F4A7C15L + z;
        double s = c - f * 0.5 + m * 2.0 - n + o * 0.25 + p - q * 4.0 + r;
        if ((a & 15) == 0) keep = new byte[(int) (a >>> 52) + 16];
        if (Thread.currentThread().isDaemon()) daemonUpcalls.incrementAndGet();
        return x ^ Double.doubleToRawLongBits(s) ^ ((Float.floatToRawIntBits(d * 3.0f) & 0xffffffffL) << 3);
    }

    public static void main(String[] args) throws Throwable {
        Fz.init("ForeignThreadAttach", args);
        System.load(System.getProperty("fz.jnilib", "/fuzz/bin/libfzjni.so"));
        init();
        // One churn round per batch (not per unit of time), so a slower run does not collect more.
        AtomicBoolean stop = new AtomicBoolean();
        Semaphore rounds = new Semaphore(0);
        Thread churn = new Thread(() -> {
            try (java.io.InputStream in = Fz.class.getResourceAsStream("/Payload.class")) {
                byte[] bytes = in.readAllBytes();
                long s = 0;
                for (int round = 0; ; round++) {
                    rounds.acquire();
                    if (stop.get()) break;
                    Class<?> c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                    var op = (java.util.function.LongUnaryOperator) c.getDeclaredConstructor().newInstance();
                    for (int i = 0; i < 20_000; i++) s += op.applyAsLong(i);
                    keep = new byte[1 << 16];
                    if (round % 8 == 7) System.gc();
                }
                keep = s;
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        }, "fz-churn");
        churn.setDaemon(true);
        churn.start();

        int batches = 40 * Fz.scale, threads = 4, calls = 64;
        long acc = 0;
        for (int b = 0; b < batches; b++) {
            rounds.release();
            long r = runBatch(b, threads, calls, Fz.seed, b % 16);
            Fz.expect(r != -1, "pthread_create");
            acc = acc * 31 + r;
        }
        Fz.out("jniBatches", batches);
        Fz.out("jniMismatches", mismatches());
        Fz.out("jniExceptions", exceptions());
        Fz.outHex("jniResult", acc);
        Fz.mix(acc);
        Fz.mix(exceptions());

        Linker linker = Linker.nativeLinker();
        MethodHandle spawn = linker.downcallHandle(SymbolLookup.loaderLookup().find("fz_ffm_threads").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT,
                                  ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        ValueLayout J = ValueLayout.JAVA_LONG, D = ValueLayout.JAVA_DOUBLE;
        FunctionDescriptor ufd = FunctionDescriptor.of(J, J, ValueLayout.JAVA_INT, D, ValueLayout.JAVA_FLOAT, J, D,
                                                       J, J, J, J, J, J, D, D, D, D, D, D);
        MethodHandle up = MethodHandles.lookup().findStatic(ForeignThreadAttach.class, "upcall", ufd.toMethodType());
        long ffmAcc = 0, ffmBad = 0;
        int ffmBatches = 20 * Fz.scale;
        for (int b = 0; b < ffmBatches; b++) {
            rounds.release();
            try (Arena arena = Arena.ofConfined()) {
                MemorySegment stub = linker.upcallStub(up, ufd, arena);
                MemorySegment bad = arena.allocate(ValueLayout.JAVA_LONG);
                long r = (long) spawn.invokeExact(stub, 4, 48, Fz.seed + b, bad);
                Fz.expect(r != -1, "pthread_create (ffm)");
                ffmAcc = ffmAcc * 31 + r;
                ffmBad += bad.get(ValueLayout.JAVA_LONG, 0);
            }
        }
        stop.set(true);
        rounds.release();
        churn.join();
        Fz.out("ffmBatches", ffmBatches);
        Fz.out("ffmMismatches", ffmBad);
        Fz.out("ffmDaemonUpcalls", daemonUpcalls.get());
        Fz.outHex("ffmResult", ffmAcc);
        Fz.mix(ffmAcc);
        Fz.expect(mismatches() == 0 && ffmBad == 0, "foreign-thread results match the native model");
        Fz.done();
    }
}
