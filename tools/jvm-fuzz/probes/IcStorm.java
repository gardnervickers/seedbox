import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.function.LongUnaryOperator;

/**
 * Inline-cache transitions while other threads run the call sites: callers invoke
 * LongUnaryOperator on shared receiver slots that a definer keeps refilling with instances of
 * freshly defined hidden classes (monomorphic, megamorphic, stub creation, unloading). Every
 * receiver computes the same function, so results do not depend on timing.
 */
public final class IcStorm {
    public static void main(String[] args) throws Exception {
        Fz.init("IcStorm", args);
        byte[] bytes;
        try (InputStream in = IcStorm.class.getResourceAsStream("/Payload.class")) { bytes = in.readAllBytes(); }
        AtomicReferenceArray<LongUnaryOperator> slots = new AtomicReferenceArray<>(8);
        for (int i = 0; i < 8; i++) slots.set(i, new Payload());
        AtomicBoolean done = new AtomicBoolean();
        Thread definer = new Thread(() -> {
            try {
                for (int i = 0; !done.get(); i++) {
                    Class<?> c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                    slots.set(i & 7, (LongUnaryOperator) c.getDeclaredConstructor().newInstance());
                    if ((i & 31) == 0) System.gc();
                }
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        });
        definer.start();
        int callers = 4;
        long[] sums = new long[callers];
        Thread[] ts = new Thread[callers];
        for (int t = 0; t < callers; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long s = 0;
                for (int i = 0; i < 400_000 * Fz.scale; i++) s += slots.get((i + id) & 7).applyAsLong(i);
                sums[id] = s;
            });
            ts[t].start();
        }
        Fz.join(ts);
        done.set(true);
        definer.join();
        for (int t = 0; t < callers; t++) Fz.outHex("caller" + t, sums[t]);
        Fz.done();
    }
}
