import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * Stack overflow while every frame holds a lock: ReentrantLock (nonfair and fair), the write
 * half of ReentrantReadWriteLock and monitors, interleaved by a seeded pattern, on threads with
 * seeded small stacks. HotSpot's reserved stack zone (@ReservedStackAccess on the AQS critical
 * sections) must keep each lock's state and owner consistent when the overflow lands inside a
 * lock or unlock: no lock may end up locked with no owner. A lock may legitimately stay held by
 * the overflowing thread (its unlock() call itself overflowed), and how many do depends on JIT
 * timing, so leaks are never printed; monitors are always released on unwinding. Each run uses
 * fresh locks, so a leak cannot block later runs.
 * Run with -XX:-PrintWarnings: whether HotSpot prints "Potentially dangerous stack overflow in
 * ReservedStackAccess annotated method" depends on which frame the overflow hits.
 */
public final class LockOverflow {
    static final int K = 64;

    static final class Run {
        final ReentrantLock[] locks = new ReentrantLock[K];
        final ReentrantReadWriteLock[] rws = new ReentrantReadWriteLock[K];
        final Object[] monitors = new Object[K];
        final byte[] pattern = new byte[K];
        long depth;

        Run(boolean fair) {
            for (int i = 0; i < K; i++) {
                locks[i] = new ReentrantLock(fair);
                rws[i] = new ReentrantReadWriteLock(fair);
                monitors[i] = new Object();
                pattern[i] = (byte) Fz.rnd.nextInt(3);
            }
        }

        void recurse(int d) {
            int i = d % K;
            depth++;
            switch (pattern[i]) {
                case 0 -> {
                    locks[i].lock();
                    try { recurse(d + 1); } finally { locks[i].unlock(); }
                }
                case 1 -> {
                    rws[i].writeLock().lock();
                    try { recurse(d + 1); } finally { rws[i].writeLock().unlock(); }
                }
                default -> {
                    synchronized (monitors[i]) { recurse(d + 1); }
                }
            }
        }

        /**
         * Locked but not owned by the only thread that ever used it (an owner never set): the
         * corruption reserved stack pages exist to prevent. Called by that thread.
         */
        int corrupt() {
            int n = 0;
            for (int i = 0; i < K; i++) {
                if (locks[i].isLocked() && !locks[i].isHeldByCurrentThread()) n++;
                if (rws[i].isWriteLocked() && !rws[i].isWriteLockedByCurrentThread()) n++;
            }
            return n;
        }

        int monitorsHeld() {
            int n = 0;
            for (Object m : monitors) if (Thread.holdsLock(m)) n++;
            return n;
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("LockOverflow", args);
        int runs = 12 * Fz.scale;
        int overflows = 0, corrupt = 0, held = 0, usable = 0;
        long maxDepth = 0;
        for (int r = 0; r < runs; r++) {
            Run run = new Run(Fz.rnd.nextBoolean());
            int start = Fz.rnd.nextInt(K);
            long stack = (256 + 64 * Fz.rnd.nextInt(8)) << 10;
            int[] result = new int[3];
            Thread t = new Thread(null, () -> {
                try {
                    run.recurse(start);
                } catch (StackOverflowError e) {
                    result[0] = 1;
                }
                result[1] = run.corrupt();
                result[2] = run.monitorsHeld();
            }, "fz-overflow", stack);
            t.start();
            t.join();
            overflows += result[0];
            corrupt += result[1];
            held += result[2];
            maxDepth = Math.max(maxDepth, run.depth);
            // Every lock the dead thread did not leak is usable by another thread.
            int ok = 0, free = 0;
            for (int i = 0; i < K; i++) {
                if (run.locks[i].isLocked()) continue;
                free++;
                if (run.locks[i].tryLock()) {
                    ok++;
                    run.locks[i].unlock();
                }
            }
            if (ok == free) usable++;
            Fz.mix(result[0] * 4 + result[1] * 2 + result[2]);
        }
        // The main thread overflows too, and keeps any lock it leaked: still reentrant for it.
        Run main = new Run(false);
        boolean mainOverflow = false;
        try {
            main.recurse(0);
        } catch (StackOverflowError e) {
            mainOverflow = true;
        }
        boolean mainConsistent = main.corrupt() == 0 && main.monitorsHeld() == 0;
        for (int i = 0; i < K; i++) while (main.locks[i].isHeldByCurrentThread()) main.locks[i].unlock();
        for (int i = 0; i < K; i++) while (main.rws[i].isWriteLockedByCurrentThread()) main.rws[i].writeLock().unlock();
        boolean mainFree = true;
        for (int i = 0; i < K; i++) mainFree &= !main.locks[i].isLocked() && !main.rws[i].isWriteLocked();
        Fz.out("overflows", overflows + "/" + runs);
        Fz.out("corruptLocks", corrupt);
        Fz.out("monitorsHeldAfterUnwind", held);
        Fz.out("unleakedLocksUsable", usable + "/" + runs);
        Fz.out("deepEnough", maxDepth > 100);
        Fz.out("main", mainOverflow + " " + mainConsistent + " " + mainFree);
        Fz.expect(overflows == runs && corrupt == 0 && held == 0 && mainConsistent && mainFree, "consistent locks after stack overflow");
        Fz.done();
    }
}
