import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import sun.misc.Unsafe;

/**
 * Recoverable resource exhaustion: soft references cleared before a heap OutOfMemoryError,
 * direct-memory reservation failure, its GC/retry path and accounting after an explicit
 * cleaner, the -XX:OnOutOfMemoryError command HotSpot forks from its error handler, and
 * metaspace exhaustion followed by class unloading. Counts of objects or classes created before
 * a failure depend on the collector and are never printed.
 * Run with -Xms32m -Xmx64m -XX:MaxDirectMemorySize=4m -XX:MaxMetaspaceSize=40m
 * -XX:OnOutOfMemoryError=touch${IFS}fz-oom-mark (the command runs through /bin/sh in the
 * working directory; ${IFS} because batch flags are split on spaces; -Xms overrides the
 * pretouch profile's -Xms384m). Without the OnOutOfMemoryError flag the marker check is
 * skipped. Optional third argument: all (default), heap, direct or metaspace.
 */
public final class OomRecovery {
    static final String MARK = "fz-oom-mark";
    static volatile Object sink;

    static boolean onOomConfigured() {
        try {
            return java.lang.management.ManagementFactory.getRuntimeMXBean().getInputArguments().stream()
                .anyMatch(a -> a.startsWith("-XX:OnOutOfMemoryError=") && a.contains(MARK));
        } catch (Throwable t) {
            return false;
        }
    }

    /** Soft references must be cleared before the collector throws OutOfMemoryError. */
    static void heap() throws Exception {
        Path mark = Path.of(MARK);
        boolean cmd = onOomConfigured();
        int softs = 4 + Fz.rnd.nextInt(5);
        List<SoftReference<byte[]>> soft = new ArrayList<>();
        for (int i = 0; i < softs; i++) soft.add(new SoftReference<>(new byte[(2 + i % 3) << 20]));
        WeakReference<Object> weak = new WeakReference<>(new long[1 << 10]);
        int chunk = 1 << (18 + Fz.rnd.nextInt(3));
        List<byte[]> keep = new ArrayList<>(1 << 12);
        boolean oom = false;
        try {
            for (;;) keep.add(new byte[chunk]);
        } catch (OutOfMemoryError e) {
            oom = true;
        }
        int held = keep.size();
        keep = null;
        int cleared = 0;
        for (SoftReference<byte[]> s : soft) if (s.get() == null) cleared++;
        Fz.out("heap.oom", oom);
        Fz.out("heap.softCleared", cleared + "/" + softs);
        Fz.out("heap.weakCleared", weak.get() == null);
        Fz.expect(oom && held > 0, "heap exhaustion");
        Fz.expect(cleared == softs, "soft references cleared before OOM");
        // The heap is usable again. (Whether a collector keeps soft references across an
        // explicit collection is its own policy; Shenandoah clears them, so that is not checked.)
        long[] big = new long[(4 << 20) / 8];
        big[big.length - 1] = 7;
        System.gc();
        Fz.out("heap.recovered", big[big.length - 1] == 7);
        Fz.mix(cleared);
        if (cmd) {
            // HotSpot runs the command, and waits for it, before the error is thrown.
            Fz.out("heap.oomCommandRan", Files.exists(mark));
            Fz.expect(Files.exists(mark), "OnOutOfMemoryError marker");
            Files.deleteIfExists(mark);
        }
    }

    static Unsafe unsafe() throws Exception {
        Field f = Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        return (Unsafe) f.get(null);
    }

    static long directUsed() {
        for (var pool : java.lang.management.ManagementFactory.getPlatformMXBeans(java.lang.management.BufferPoolMXBean.class))
            if (pool.getName().equals("direct")) return pool.getMemoryUsed();
        return -1;
    }

    /** Direct-memory accounting: failure, the GC/retry path for unreachable buffers, explicit cleaners. */
    static void direct() throws Exception {
        long max = 4L << 20;
        Unsafe u = unsafe();
        long base = directUsed();
        int avail = (int) (max - base), half = avail / 2, slot = avail - half - 4096;
        ByteBuffer b = ByteBuffer.allocateDirect(half), a = ByteBuffer.allocateDirect(slot);
        b.putLong(0, 2);
        a.putLong(0, 1);
        int fails = 0;
        try {
            sink = ByteBuffer.allocateDirect(8192);
        } catch (OutOfMemoryError e) {
            fails++;
        }
        Fz.out("direct.reachableFails", fails);
        Fz.expect(fails == 1, "direct reservation beyond MaxDirectMemorySize fails");
        // Explicit release: the accounting goes down at once and the space is reusable.
        u.invokeCleaner(a);
        a = ByteBuffer.allocateDirect(slot);
        a.putLong(slot - 8, 4);
        Fz.out("direct.afterCleaner", a.getLong(slot - 8));
        // Unreachable buffers: Bits.reserveMemory must collect, process references and retry.
        int rounds = 2 + Fz.rnd.nextInt(3);
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            a = null;
            a = ByteBuffer.allocateDirect(slot);
            a.putLong(0, r + 10);
            sum += a.getLong(0);
        }
        Fz.out("direct.retryRounds", rounds);
        sum += b.getLong(0);
        Fz.out("direct.sum", sum);
        Fz.mix(sum);
        u.invokeCleaner(a);
        u.invokeCleaner(b);
        a = b = null;
        // Every live buffer has been cleaned explicitly, so the accounting is back at base.
        Fz.out("direct.accountingRestored", directUsed() == base);
        ByteBuffer whole = ByteBuffer.allocateDirect(avail);
        Fz.out("direct.wholeLimit", whole.capacity() == avail);
        u.invokeCleaner(whole);
        Fz.expect(directUsed() == base, "direct accounting after cleaners");
    }

    public interface Probe extends Callable<Integer> {}

    /** Fills metaspace with proxy classes, each in its own loader, then unloads them. */
    static void metaspace() throws Exception {
        InvocationHandler h = (p, m, a) -> m.getName().equals("call") ? 41 : null;
        List<Object> keep = new ArrayList<>(1 << 14);
        boolean oom = false;
        try {
            for (;;) {
                ClassLoader l = new URLClassLoader(new URL[0], OomRecovery.class.getClassLoader());
                keep.add(Proxy.newProxyInstance(l, new Class<?>[] {Probe.class}, h));
            }
        } catch (OutOfMemoryError e) {
            oom = e.getMessage() != null && e.getMessage().contains("Metaspace");
        }
        int made = keep.size();
        WeakReference<Class<?>> first = new WeakReference<>(keep.get(0).getClass());
        int calls = 0;
        for (int i = 0; i < 16; i++) calls += ((Probe) keep.get(Fz.rnd.nextInt(made))).call();
        keep.clear();
        keep = null;
        Fz.out("meta.oom", oom);
        Fz.out("meta.calls", calls);
        Fz.expect(oom, "metaspace exhaustion");
        boolean unloaded = false;
        for (int i = 0; i < 20 && !unloaded; i++) {
            System.gc();
            unloaded = first.get() == null;
        }
        Fz.out("meta.unloaded", unloaded);
        // Metaspace is reusable: as many classes again as before, minus a margin.
        int again = 0;
        List<Object> second = new ArrayList<>();
        for (int i = 0; i < made / 2; i++) {
            ClassLoader l = new URLClassLoader(new URL[0], OomRecovery.class.getClassLoader());
            second.add(Proxy.newProxyInstance(l, new Class<?>[] {Probe.class}, h));
            again++;
        }
        Fz.out("meta.recovered", again == made / 2 && ((Probe) second.get(again - 1)).call() == 41);
        Fz.mix(calls);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("OomRecovery", args);
        String mode = args.length > 2 ? args[2] : "all";
        Files.deleteIfExists(Path.of(MARK));
        if (mode.equals("all") || mode.equals("direct")) direct();
        if (mode.equals("all") || mode.equals("heap")) heap();
        if (mode.equals("all") || mode.equals("metaspace")) metaspace();
        Files.deleteIfExists(Path.of(MARK));
        Fz.done();
    }
}
