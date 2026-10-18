import java.lang.ref.Cleaner;
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/** Reference processing: queues, phantom references, Cleaner actions and soft references under pressure. */
public final class References {
    public static void main(String[] args) throws Exception {
        Fz.init("References", args);
        int n = 2_000 * Fz.scale;
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        List<Reference<Object>> refs = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            Object o = new byte[32 + (i & 63)];
            refs.add((i & 1) == 0 ? new WeakReference<>(o, q) : new PhantomReference<>(o, q));
        }
        Cleaner cleaner = Cleaner.create();
        AtomicInteger cleaned = new AtomicInteger();
        for (int i = 0; i < n; i++) cleaner.register(new Object(), cleaned::incrementAndGet);
        int dequeued = 0;
        long deadline = System.nanoTime() + 60_000_000_000L;
        while ((dequeued < n || cleaned.get() < n) && System.nanoTime() < deadline) {
            System.gc();
            Reference<?> r;
            while ((r = q.remove(10)) != null) dequeued++;
        }
        Fz.out("dequeued", dequeued);
        Fz.out("cleaned", cleaned.get());
        SoftReference<long[]> soft = new SoftReference<>(new long[1 << 16]);
        System.gc();
        Fz.out("softSurvivesGc", soft.get() != null);
        refs.clear();
        Fz.done();
    }
}
