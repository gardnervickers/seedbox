import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.ConcurrentLinkedQueue;

/**
 * Deliberately schedule- and entropy-dependent output. Natively it changes from run to run;
 * a simulation and its replay must print exactly the same bytes.
 */
public final class Race {
    static int racy;

    public static void main(String[] args) throws Exception {
        Fz.init("Race", args);
        int n = 4;
        ConcurrentLinkedQueue<Integer> order = new ConcurrentLinkedQueue<>();
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 200_000 * Fz.scale; i++) {
                    racy++;
                    if ((i & 8191) == 0) order.add(id);
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        Fz.out("racy", racy);
        Fz.out("order", order.hashCode() + " " + order.stream().map(String::valueOf).reduce("", String::concat).hashCode());
        List<Object> objs = new ArrayList<>();
        for (int i = 0; i < 100; i++) objs.add(new Object());
        long ih = 0;
        for (Object o : objs) ih = ih * 31 + System.identityHashCode(o);
        Fz.outHex("identityHash", ih);
        Set<Object> set = new HashSet<>(objs);
        Fz.out("hashSetOrder", set.iterator().next() == objs.get(0));
        Fz.out("random", Math.random() + " " + UUID.randomUUID() + " " + new java.security.SecureRandom().nextLong());
        Fz.out("nanoTime", System.nanoTime() % 1_000_000_007L);
        Fz.out("currentTimeMillis", System.currentTimeMillis());
        Fz.out("pid", ProcessHandle.current().pid());
        Fz.done();
    }
}
