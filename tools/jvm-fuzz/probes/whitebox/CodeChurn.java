import java.lang.reflect.Method;
import jdk.test.whitebox.WhiteBox;

/**
 * Exact code modification through HotSpot's WhiteBox API while workers run: deoptimize all
 * compiled code, recompile chosen methods at C1 or C2, clear inline caches, walk worker stacks
 * in handshakes (Thread.getStackTrace), allocate and free code blobs, and collect. Worker results must not depend on
 * which version of the code ran.
 */
public final class CodeChurn {
    static long a(long x) { return x * 0x9E3779B97F4A7C15L + 7; }
    static long b(long x) { return Long.rotateLeft(x, 13) ^ (x >>> 7); }
    static long c(long x) { return x % 1_000_003 + (x & 0xff) * 31; }
    static long d(long x) { long h = x; for (int i = 0; i < 4; i++) h = h * 31 + i; return h; }
    static long mix(long x) { return a(x) + b(x) - c(x) ^ d(x); }

    public static void main(String[] args) throws Exception {
        Fz.init("CodeChurn", args);
        WhiteBox wb = WhiteBox.getWhiteBox();
        Method[] hot = {
            CodeChurn.class.getDeclaredMethod("a", long.class), CodeChurn.class.getDeclaredMethod("b", long.class),
            CodeChurn.class.getDeclaredMethod("c", long.class), CodeChurn.class.getDeclaredMethod("d", long.class),
            CodeChurn.class.getDeclaredMethod("mix", long.class),
        };
        // Compilation levels this VM accepts: C1 (1) and C2 (4) unless tiering or compilers are off.
        boolean tiered = wb.getBooleanVMFlag("TieredCompilation");
        long stop = wb.getIntxVMFlag("TieredStopAtLevel");
        int[] levels = !wb.getBooleanVMFlag("UseCompiler") ? new int[0]
            : !tiered ? new int[] {4} : stop < 4 ? new int[] {1} : new int[] {1, 4};
        int workers = 3;
        long[] sums = new long[workers];
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long s = 0;
                for (long i = 0; i < 4_000_000L * Fz.scale; i++) s += mix(i + id);
                sums[id] = s;
            });
            ts[w].start();
        }
        int rounds = 0;
        while (ts[0].isAlive() || ts[1].isAlive() || ts[2].isAlive()) {
            wb.deoptimizeAll();
            for (int i = 0; i < hot.length && levels.length > 0; i++)
                wb.enqueueMethodForCompilation(hot[i], levels[(rounds + i) % levels.length]);
            if (rounds % 3 == 0) wb.clearInlineCaches();
            for (Thread t : ts) t.getStackTrace();
            long blob = wb.allocateCodeBlob(4096, 2);
            if (blob != 0) wb.freeCodeBlob(blob);
            if (rounds % 10 == 0) wb.youngGC();
            if (rounds % 25 == 0) wb.fullGC();
            wb.forceSafepoint();
            rounds++;
            Thread.sleep(2);
        }
        Fz.join(ts);
        Fz.expect(rounds > 0, "at least one churn round");
        long expect = 0;
        for (long i = 0; i < 4_000_000L * Fz.scale; i++) expect += mix(i);
        Fz.out("worker0Matches", sums[0] == expect);
        for (int w = 0; w < workers; w++) Fz.outHex("worker" + w, sums[w]);
        Fz.done();
    }
}
