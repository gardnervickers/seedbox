import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentSkipListMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.stream.Collectors;

/** Collection implementations driven by one random operation sequence, compared against each other, plus streams. */
public final class Collections {
    enum Op { A, B, C, D }

    public static void main(String[] args) {
        Fz.init("Collections", args);
        int n = 200_000 * Fz.scale;
        List<Map<Integer, Integer>> maps = List.of(new HashMap<>(), new TreeMap<>(), new LinkedHashMap<>(), new ConcurrentHashMap<>(),
            new ConcurrentSkipListMap<>(), new Hashtable<>(), new WeakHashMap<>());
        List<Collection<Integer>> lists = List.of(new ArrayList<>(), new LinkedList<>(), new ArrayDeque<>(), new CopyOnWriteArrayList<>(), new Vector<>());
        PriorityQueue<Integer> pq = new PriorityQueue<>();
        BitSet bits = new BitSet();
        EnumMap<Op, Long> em = new EnumMap<>(Op.class);
        Integer[] keys = new Integer[512];
        for (int i = 0; i < keys.length; i++) keys[i] = i * 7919 - 100_000;
        for (int i = 0; i < n; i++) {
            Integer k = keys[Fz.rnd.nextInt(keys.length)];
            int op = Fz.rnd.nextInt(10);
            for (Map<Integer, Integer> m : maps) {
                if (op < 6) m.merge(k, i, Integer::sum);
                else if (op < 8) m.remove(k);
                else m.computeIfAbsent(k, x -> x * 2);
            }
            if (i % 20 == 0) {
                for (Collection<Integer> c : lists) {
                    if (c instanceof Deque<Integer> d && (op & 1) == 0) d.pollFirst();
                    else if (c instanceof List<Integer> l && op < 3 && !l.isEmpty()) l.remove(l.size() / 2);
                    else c.add(k);
                }
            }
            pq.add(k);
            if (pq.size() > 1000) pq.poll();
            bits.flip(k & 0xffff);
            em.merge(Op.values()[op & 3], (long) k, Long::sum);
        }
        Map<Integer, Integer> ref = new TreeMap<>(maps.get(0));
        for (Map<Integer, Integer> m : maps) Fz.expect(new TreeMap<>(m).equals(ref), m.getClass().getSimpleName() + " diverged");
        Fz.out("maps", ref.size() + " " + ref.hashCode());
        Fz.out("hashOrder", maps.get(0).keySet().stream().limit(8).toList());
        for (Collection<Integer> c : lists) Fz.out(c.getClass().getSimpleName(), c.size() + " " + new ArrayList<>(c).hashCode());
        Fz.out("pq", pq.size() + " " + pq.peek());
        Fz.out("bits", bits.cardinality() + " " + bits.length() + " " + Arrays.hashCode(bits.toLongArray()));
        Fz.out("enumMap", em);
        Map<Boolean, Map<Integer, Long>> grouped = ref.entrySet().stream()
            .collect(Collectors.partitioningBy(e -> e.getValue() % 2 == 0, Collectors.groupingBy(e -> e.getKey() & 7, TreeMap::new, Collectors.counting())));
        Fz.out("grouped", grouped);
        Fz.out("stream", ref.values().stream().filter(v -> v > 0).map(v -> v % 1000).sorted(Comparator.reverseOrder()).distinct().limit(5).collect(Collectors.toList()));
        List<String> strs = new ArrayList<>();
        for (int i = 0; i < 1000; i++) strs.add(Integer.toString(Fz.rnd.nextInt(100_000), 36));
        strs.sort(Comparator.comparing(String::length).thenComparing(Comparator.reverseOrder()));
        Fz.out("sorted", strs.subList(0, 5));
        Fz.out("immutable", List.of(1, 2, 3).equals(Arrays.asList(1, 2, 3)) + " " + Set.of("a").contains("a") + " " + Map.of(1, 2).get(1));
        Fz.done();
    }
}
