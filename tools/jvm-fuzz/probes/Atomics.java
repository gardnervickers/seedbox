import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.LongAccumulator;
import java.util.concurrent.atomic.LongAdder;

/** Contended CAS, fetch-and-add and exchange loops (LSE or exclusive-monitor sequences). */
public final class Atomics {
    volatile long vfield;
    int plain;
    byte small;
    static final VarHandle VF, PL, SM, ARR;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VF = l.findVarHandle(Atomics.class, "vfield", long.class);
            PL = l.findVarHandle(Atomics.class, "plain", int.class);
            SM = l.findVarHandle(Atomics.class, "small", byte.class);
            ARR = MethodHandles.arrayElementVarHandle(long[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    record Pair(int a, int b) {}

    public static void main(String[] args) throws Exception {
        Fz.init("Atomics", args);
        int n = 2 + (int) (Fz.seed % 4);
        int per = 100_000 * Fz.scale;
        AtomicInteger ai = new AtomicInteger();
        AtomicLong al = new AtomicLong();
        AtomicReference<Pair> ar = new AtomicReference<>(new Pair(0, 0));
        AtomicIntegerArray aia = new AtomicIntegerArray(16);
        LongAdder adder = new LongAdder();
        LongAccumulator max = new LongAccumulator(Math::max, Long.MIN_VALUE);
        Atomics obj = new Atomics();
        long[] cells = new long[8];
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < per; i++) {
                    ai.incrementAndGet();
                    al.addAndGet(i);
                    int v;
                    do { v = ai.get(); } while (!ai.weakCompareAndSetVolatile(v, v));
                    aia.getAndAdd(i & 15, 1);
                    adder.increment();
                    max.accumulate(i * (long) (id + 1));
                    VF.getAndAdd(obj, 3L);
                    int p;
                    do { p = (int) PL.getVolatile(obj); } while (!PL.compareAndSet(obj, p, p + 1));
                    SM.getAndBitwiseXor(obj, (byte) 1);
                    ARR.getAndAdd(cells, i & 7, 1L);
                    if ((i & 63) == 0) {
                        Pair old, nu;
                        do { old = ar.get(); nu = new Pair(old.a() + 1, old.b() + id); } while (!ar.compareAndSet(old, nu));
                    }
                    long x;
                    do { x = (long) VF.getAcquire(obj); } while ((long) VF.compareAndExchange(obj, x, x) != x);
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        long cellSum = 0;
        for (long c : cells) cellSum += c;
        Fz.out("ai", ai.get());
        Fz.out("al", al.get());
        Fz.out("aia", aia.toString());
        Fz.out("adder", adder.sum() + " " + max.get());
        Fz.out("vh", obj.vfield + " " + obj.plain + " " + obj.small + " " + cellSum);
        Fz.out("ref", ar.get());
        Fz.expect(ai.get() == n * per && obj.plain == n * per, "lost update");
        Fz.done();
    }
}
