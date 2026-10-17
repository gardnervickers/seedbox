import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentSkipListMap;

/**
 * One map implementation filled by a random merge/remove sequence, then copied into TreeMaps.
 * Optional third argument: the implementation (hash, tree, linked, chm, skiplist, hashtable, weak).
 */
public final class MapCopy {
    public static void main(String[] args) {
        Fz.init("MapCopy", args);
        String kind = args.length > 2 ? args[2] : "hash";
        Map<Integer, Integer> m = switch (kind) {
            case "tree" -> new TreeMap<>();
            case "linked" -> new LinkedHashMap<>();
            case "chm" -> new ConcurrentHashMap<>();
            case "skiplist" -> new ConcurrentSkipListMap<>();
            case "hashtable" -> new Hashtable<>();
            case "weak" -> new WeakHashMap<>();
            default -> new HashMap<>();
        };
        Integer[] keys = new Integer[512];
        for (int i = 0; i < keys.length; i++) keys[i] = i * 7919 - 100_000;
        long copies = 0;
        for (int i = 0; i < 200_000 * Fz.scale; i++) {
            Integer k = keys[Fz.rnd.nextInt(keys.length)];
            int op = Fz.rnd.nextInt(10);
            if (op < 6) m.merge(k, i, Integer::sum);
            else if (op < 8) m.remove(k);
            else m.computeIfAbsent(k, x -> x * 2);
            if (i % 5_000 == 0) copies += new TreeMap<>(m).hashCode();
        }
        TreeMap<Integer, Integer> t = new TreeMap<>(m);
        Fz.out("map", kind + " " + t.size() + " " + t.hashCode() + " " + copies);
        Fz.done();
    }
}
