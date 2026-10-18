import java.util.ArrayList;
import java.util.List;

/**
 * Fifty threads sleep k * 5 ms and append their k: many pending timers and futex wake-ups that
 * must fire in order. Natively a loaded host can reorder close deadlines, so only the
 * simulation and its replay are compared.
 */
public final class SleepSort {
    public static void main(String[] args) throws Exception {
        Fz.init("SleepSort", args);
        int n = 50;
        List<Integer> order = new ArrayList<>();
        Thread[] ts = new Thread[n];
        int[] ks = new int[n];
        for (int i = 0; i < n; i++) ks[i] = i;
        for (int i = n - 1; i > 0; i--) { int j = Fz.rnd.nextInt(i + 1); int t = ks[i]; ks[i] = ks[j]; ks[j] = t; }
        for (int i = 0; i < n; i++) {
            final int k = ks[i];
            ts[i] = new Thread(() -> {
                try { Thread.sleep(20 + k * 5L); } catch (InterruptedException e) { throw new RuntimeException(e); }
                synchronized (order) { order.add(k); }
            });
        }
        for (Thread t : ts) t.start();
        Fz.join(ts);
        boolean sorted = true;
        for (int i = 0; i < n; i++) sorted &= order.get(i) == i;
        Fz.out("sorted", sorted);
        Fz.done();
    }
}
