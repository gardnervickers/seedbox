import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.function.LongUnaryOperator;

/**
 * Code-cache reuse: in a small unsegmented code cache, hidden copies of a class with a large
 * method are compiled, dropped and collected, so new code lands where freed code was while a
 * reader thread keeps calling the live copies. The translator must drop its translations of
 * the reused addresses.
 */
public final class CodeReuse {
    public static void main(String[] args) throws Exception {
        Fz.init("CodeReuse", args);
        byte[] bytes;
        try (InputStream in = CodeReuse.class.getResourceAsStream("/BigPayload.class")) { bytes = in.readAllBytes(); }
        LongUnaryOperator reference = new BigPayload();
        long[] mismatches = {0};
        List<WeakReference<Class<?>>> dropped = new ArrayList<>();
        LongUnaryOperator[] live = {reference};
        Thread reader = new Thread(() -> {
            for (long i = 0; i < 3_000_000L * Fz.scale; i++) {
                if (live[0].applyAsLong(i) != reference.applyAsLong(i)) mismatches[0]++;
            }
        });
        reader.start();
        int rounds = 0;
        while (reader.isAlive()) {
            Class<?> c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
            LongUnaryOperator op = (LongUnaryOperator) c.getDeclaredConstructor().newInstance();
            for (int i = 0; i < 30_000; i++) op.applyAsLong(i);
            live[0] = op;
            dropped.add(new WeakReference<>(c));
            if (++rounds % 10 == 0) System.gc();
        }
        reader.join();
        live[0] = reference;
        // A running compilation keeps every hidden class it touches (the holder of the method
        // and the profiled receivers it inlines) alive until it ends; queued tasks hold theirs
        // weakly. When the reader ends, a C2 compilation of this loop often still holds the
        // newest copies, and three back-to-back collections can finish before it does,
        // above all in a slow guest. Collect again while compilations finish.
        long unloaded = 0;
        for (int i = 0; i < 50; i++) {
            System.gc();
            unloaded = dropped.stream().filter(w -> w.get() == null).count();
            if (i >= 2 && unloaded >= dropped.size() - 2) break;
            if (i >= 2) Thread.sleep(100);
        }
        Fz.out("mismatches", mismatches[0]);
        Fz.out("mostUnloaded", unloaded >= dropped.size() - 2);
        Fz.done();
    }
}
