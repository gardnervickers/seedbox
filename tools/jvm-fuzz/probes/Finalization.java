import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Finalizers: resurrection published by the finalizer thread, a resurrected object that is never
 * finalized twice, finalizers that throw, and a batch of finalizable objects. Run also with
 * --finalization=disabled; then no finalizer thread exists and no callback may run.
 */
public final class Finalization {
    static final AtomicInteger finalized = new AtomicInteger(), thrown = new AtomicInteger();
    static final List<Finalization> saved = new ArrayList<>();
    static final Object lock = new Object();

    final int id;
    final boolean resurrect, fail;
    final long[] payload;
    int finalizations;

    Finalization(int id, boolean resurrect, boolean fail) {
        this.id = id;
        this.resurrect = resurrect;
        this.fail = fail;
        this.payload = new long[1 + (id & 15)];
        payload[0] = id * 31L;
    }

    @Override
    @SuppressWarnings({"deprecation", "removal"})
    protected void finalize() {
        finalizations++;
        if (resurrect) {
            synchronized (lock) {
                saved.add(this);
                lock.notifyAll();
            }
        }
        // Counted last: once the main thread sees every finalization, all other effects are visible.
        if (fail) {
            thrown.incrementAndGet();
            finalized.incrementAndGet();
            throw new IllegalStateException("ignored by the finalizer thread");
        }
        finalized.incrementAndGet();
    }

    static boolean finalizerThread() {
        return Thread.getAllStackTraces().keySet().stream().anyMatch(t -> t.getName().equals("Finalizer"));
    }

    static void make(int n, int resurrectEvery, int failEvery) {
        for (int i = 0; i < n; i++)
            new Finalization(i, resurrectEvery > 0 && i % resurrectEvery == 0, failEvery > 0 && i % failEvery == 1);
    }

    @SuppressWarnings({"deprecation", "removal"})
    static void collectUntil(int target) throws InterruptedException {
        for (int i = 0; i < 400 && finalized.get() < target; i++) {
            System.gc();
            System.runFinalization();
            if (finalized.get() < target) Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Finalization", args);
        boolean enabled = finalizerThread();
        Fz.out("finalizerThread", enabled);
        int n = 500 + Fz.rnd.nextInt(500);
        int every = 3 + Fz.rnd.nextInt(5);
        int failEvery = 4 + Fz.rnd.nextInt(4);
        int resurrected = (n + every - 1) / every;
        make(n, every, failEvery);
        Fz.mix(n);
        if (!enabled) {
            // Nothing may ever run: give a collection the chance and observe zero callbacks.
            System.gc();
            Fz.out("finalized", finalized.get());
            Fz.expect(finalized.get() == 0, "finalization disabled");
            Fz.done();
            return;
        }
        collectUntil(n);
        Fz.out("finalized", finalized.get() + "/" + n);
        Fz.out("thrown", thrown.get());
        Fz.expect(finalized.get() == n, "every finalizer ran");
        long sum = 0;
        synchronized (lock) {
            Fz.out("resurrected", saved.size() + "/" + resurrected);
            Fz.expect(saved.size() == resurrected, "resurrection published");
            for (Finalization f : saved) {
                Fz.expect(f.finalizations == 1 && f.payload[0] == f.id * 31L, "resurrected state");
                sum += f.payload[0];
            }
            // Drop them again: a resurrected object is never finalized a second time.
            saved.clear();
        }
        Fz.out("resurrectedSum", sum);
        Fz.mix(sum);
        int before = finalized.get();
        // A second, smaller generation proves the finalizer thread still runs, and that none
        // of the first generation runs again.
        int m = 50 + Fz.rnd.nextInt(50);
        make(m, 0, 0);
        collectUntil(before + m);
        for (int i = 0; i < 3; i++) {
            System.gc();
            System.runFinalization();
        }
        Fz.out("secondGeneration", finalized.get() - before);
        Fz.expect(finalized.get() == before + m, "no finalizer ran twice");
        Fz.mix(finalized.get());
        Fz.done();
    }
}
