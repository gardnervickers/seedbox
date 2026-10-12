import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Allocation churn, object graphs that survive moving collections, large arrays and weak references. */
public final class GcStress {
    static final class Node {
        final long v;
        Node l, r;
        int[] pad;
        Node(long v) { this.v = v; this.pad = new int[(int) (v & 15)]; }
    }

    static Node tree(int depth, long v) {
        Node n = new Node(v);
        if (depth > 0) {
            n.l = tree(depth - 1, v * 2 + 1);
            n.r = tree(depth - 1, v * 2 + 2);
        }
        return n;
    }

    static long walk(Node n) {
        if (n == null) return 0;
        return n.v * 31 + n.pad.length + walk(n.l) ^ walk(n.r) * 7;
    }

    public static void main(String[] args) {
        Fz.init("GcStress", args);
        List<Node> keep = new ArrayList<>();
        Map<Integer, long[]> map = new HashMap<>();
        long garbage = 0;
        for (int round = 0; round < 8 * Fz.scale; round++) {
            Node t = tree(10 + (round & 3), round);
            keep.add(t);
            if (keep.size() > 4) keep.remove(0);
            for (int i = 0; i < 20_000; i++) {
                Object o = (i % 3 == 0) ? new byte[64 + (i & 255)] : (i % 3 == 1) ? new long[i & 31] : new Object[] {Integer.valueOf(i)};
                garbage += o.getClass().getName().length();
            }
            long[] big = new long[(1 << 18) + round];
            for (int i = 0; i < big.length; i += 97) big[i] = i * (long) round;
            map.put(round, big);
            if (map.size() > 3) map.remove(round - 3);
            if (round % 3 == 2) System.gc();
            long w = 0;
            for (Node n : keep) w += walk(n);
            Fz.mix(w);
        }
        for (long[] big : map.values()) {
            long s = 0;
            for (int i = 0; i < big.length; i += 97) s += big[i];
            Fz.mix(s);
        }
        Fz.mix(garbage);
        Object referent = new int[1024];
        WeakReference<Object> weak = new WeakReference<>(referent);
        referent = null;
        int gcs = 0;
        while (weak.get() != null && gcs < 20) {
            System.gc();
            gcs++;
        }
        Fz.out("weakCleared", weak.get() == null);
        Fz.done();
    }
}
