import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The foreign-function API's generated stubs: native-to-Java upcalls (qsort with a Java
 * comparator), variadic downcalls (snprintf), a struct returned by value (div), errno
 * captured across the call, and a shared arena closed while another thread reads it.
 */
public final class Ffm {
    static final Linker L = Linker.nativeLinker();

    static MethodHandle fn(String name, FunctionDescriptor fd, Linker.Option... opts) {
        return L.downcallHandle(L.defaultLookup().find(name).orElseThrow(), fd, opts);
    }

    static int compare(MemorySegment x, MemorySegment y) {
        return Integer.compare(x.reinterpret(4).get(ValueLayout.JAVA_INT, 0), y.reinterpret(4).get(ValueLayout.JAVA_INT, 0));
    }

    public static void main(String[] args) throws Throwable {
        Fz.init("Ffm", args);
        MethodHandle qsort = fn("qsort", FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        MethodHandle cmp = MethodHandles.lookup().findStatic(Ffm.class, "compare",
            MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        long sortedSum = 0;
        try (Arena a = Arena.ofConfined()) {
            MemorySegment stub = L.upcallStub(cmp, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS), a);
            for (int r = 0; r < 200 * Fz.scale; r++) {
                int n = 64;
                MemorySegment data = a.allocateArray(ValueLayout.JAVA_INT, n);
                for (int i = 0; i < n; i++) data.setAtIndex(ValueLayout.JAVA_INT, i, Fz.rnd.nextInt());
                qsort.invokeExact(data, (long) n, 4L, stub);
                boolean ok = true;
                for (int i = 1; i < n; i++) ok &= data.getAtIndex(ValueLayout.JAVA_INT, i - 1) <= data.getAtIndex(ValueLayout.JAVA_INT, i);
                Fz.expect(ok, "qsort order");
                sortedSum += data.getAtIndex(ValueLayout.JAVA_INT, 0);
            }
            MethodHandle snprintf = fn("snprintf", FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS, ValueLayout.JAVA_INT, ValueLayout.JAVA_DOUBLE, ValueLayout.JAVA_LONG), Linker.Option.firstVariadicArg(3));
            MemorySegment out = a.allocate(128), fmt = a.allocateUtf8String("%d/%.3f/%ld");
            int len = (int) snprintf.invokeExact(out, 128L, fmt, 7, 2.5, 1L << 40);
            Fz.out("snprintf", out.getUtf8String(0) + " " + len);
            StructLayout divT = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("quot"), ValueLayout.JAVA_INT.withName("rem"));
            MethodHandle div = fn("div", FunctionDescriptor.of(divT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
            MemorySegment q = (MemorySegment) div.invokeExact((java.lang.foreign.SegmentAllocator) a, 47, 5);
            Fz.out("div", q.get(ValueLayout.JAVA_INT, 0) + " " + q.get(ValueLayout.JAVA_INT, 4));
            StructLayout state = Linker.Option.captureStateLayout();
            VarHandle errno = state.varHandle(MemoryLayout.PathElement.groupElement("errno"));
            MethodHandle close = fn("close", FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), Linker.Option.captureCallState("errno"));
            MemorySegment st = a.allocate(state);
            int rc = (int) close.invokeExact(st, -1);
            Fz.out("closeErrno", rc + " " + (int) errno.get(st));
        }
        Fz.out("sortedSum", sortedSum);
        AtomicLong seen = new AtomicLong();
        int closedSeen = 0;
        for (int r = 0; r < 50 * Fz.scale; r++) {
            Arena shared = Arena.ofShared();
            MemorySegment seg = shared.allocate(4096);
            Thread reader = new Thread(() -> {
                try {
                    while (true) seen.addAndGet(seg.get(ValueLayout.JAVA_BYTE, 7));
                } catch (IllegalStateException e) {
                    seen.addAndGet(1_000_000);
                }
            });
            reader.start();
            Thread.sleep(1);
            // Closing hands off to every thread; while one is mid-access the close is refused.
            while (true) {
                try { shared.close(); break; } catch (IllegalStateException busy) { Thread.onSpinWait(); }
            }
            reader.join();
            closedSeen++;
        }
        Fz.out("sharedClosed", closedSeen + " " + (seen.get() / 1_000_000));
        Fz.done();
    }
}
