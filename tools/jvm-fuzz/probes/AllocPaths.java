import com.sun.management.HotSpotDiagnosticMXBean;
import java.lang.management.ManagementFactory;
import java.lang.reflect.Field;
import java.util.Collections;
import java.util.IdentityHashMap;
import java.util.Set;

/**
 * Allocation and collector paths chosen by flags (one per run, see the variants in jvmfuzz.py):
 * -XX:-UseTLAB sends every allocation through the shared eden slow path, contended here by three
 * threads; -XX:+UseEpsilonGC commits and zeroes heap without ever collecting; and
 * -XX:+UseStringDeduplication lets the deduplication thread rewrite live String.value fields
 * while the program checks every string. The work fits in an uncollected 384 MB heap.
 */
public final class AllocPaths {
    static boolean flag(HotSpotDiagnosticMXBean mx, String name) {
        try {
            return Boolean.parseBoolean(mx.getVMOption(name).getValue());
        } catch (IllegalArgumentException e) {
            return false;
        }
    }

    /** Rows of small escaping objects, from several threads at once. */
    static long churn(long salt) {
        Object[][] roots = new Object[8][];
        long n = 0;
        for (int r = 0; r < 300; r++) {
            Object[] row = new Object[500];
            for (int i = 0; i < row.length; i++) {
                switch (i & 3) {
                    case 0 -> row[i] = new byte[17 + (i & 7)];
                    case 1 -> row[i] = Long.valueOf(salt + i * 1_000_003L);
                    case 2 -> row[i] = new int[] {r, i};
                    default -> row[i] = new Object();
                }
            }
            roots[r & 7] = row;
        }
        for (Object[] row : roots) {
            for (Object o : row) {
                if (o instanceof byte[] b) n += b.length;
                else if (o instanceof Long l) n += l - salt;
                else if (o instanceof int[] a) n += a[0] + a[1];
                else n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("AllocPaths", args);
        HotSpotDiagnosticMXBean mx = ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        boolean dedup = flag(mx, "UseStringDeduplication"), epsilon = flag(mx, "UseEpsilonGC");
        Fz.out("mode", "UseTLAB=" + flag(mx, "UseTLAB") + " UseEpsilonGC=" + epsilon + " UseStringDeduplication=" + dedup);

        long[] results = new long[3];
        long[] salts = {Fz.rnd.nextLong(1 << 30), Fz.rnd.nextLong(1 << 30), Fz.rnd.nextLong(1 << 30)};
        Thread[] ts = new Thread[2];
        for (int t = 0; t < 2; t++) {
            final int k = t + 1;
            ts[t] = new Thread(() -> results[k] = churn(salts[k]));
            ts[t].start();
        }
        results[0] = churn(salts[0]);
        Fz.join(ts);
        Fz.out("churn", results[0] + " " + results[1] + " " + results[2]);
        Fz.expect(results[0] == results[1] && results[1] == results[2], "churn agrees across threads");
        Fz.mix(results[0]);

        byte[][] roots = new byte[8192][];
        for (int i = 0; i < roots.length; i++) {
            roots[i] = new byte[1024];
            roots[i][0] = (byte) i;
            roots[i][1023] = (byte) (i * 7 + Fz.seed);
        }
        byte[][] big = new byte[6][];
        for (int i = 0; i < big.length; i++) {
            big[i] = new byte[(1 << 20) * (1 + i % 3) + 8];
            for (int j = 0; j < big[i].length; j += 4093) big[i][j] = (byte) (j ^ i ^ Fz.seed);
        }
        System.gc();
        long sum = 0, tail = 0, bigSum = 0;
        for (byte[] b : roots) {
            sum += b[0];
            tail += b[1023];
            for (int j = 1; j < 1023; j += 97) Fz.expect(b[j] == 0, "fresh arrays are zeroed");
        }
        for (int i = 0; i < big.length; i++) {
            for (int j = 0; j < big[i].length; j++) {
                byte want = j % 4093 == 0 ? (byte) (j ^ i ^ Fz.seed) : 0;
                if (big[i][j] != want) Fz.expect(false, "large array content");
                bigSum += big[i][j];
            }
        }
        Fz.out("roots", sum + " " + tail + " " + bigSum);
        Fz.mix(tail);
        Fz.mix(bigSum);
        roots = null;
        big = null;

        int groups = 4, per = 5_000;
        String[] texts = new String[groups];
        for (int g = 0; g < groups; g++) texts[g] = "dedup-" + Fz.seed + "-" + g + "-" + "x".repeat(Fz.rnd.nextInt(40));
        String[] keep = new String[groups * per];
        for (int i = 0; i < keep.length; i++) keep[i] = new String(texts[i % groups].toCharArray());
        Field value = null;
        try {
            value = String.class.getDeclaredField("value");
            value.setAccessible(true);
        } catch (RuntimeException e) {
            value = null; // without --add-opens java.base/java.lang=ALL-UNNAMED only contents are checked
        }
        boolean converged = false;
        for (int round = 0; round < (dedup && value != null ? 300 : 10) && !converged; round++) {
            System.gc();
            Thread.sleep(dedup ? 20 : 1);
            for (int i = 0; i < keep.length; i++) Fz.expect(keep[i].equals(texts[i % groups]), "string contents during deduplication");
            if (dedup && value != null) {
                Set<Object> arrays = Collections.newSetFromMap(new IdentityHashMap<>());
                for (String s : keep) arrays.add(value.get(s));
                converged = arrays.size() == groups;
            }
        }
        long h = 0;
        for (String s : keep) h = h * 31 + s.hashCode();
        Fz.out("strings", keep.length + " " + Long.toHexString(h));
        if (dedup && value != null) Fz.out("dedupConverged", converged);
        Fz.mix(h);
        Fz.done();
    }
}
