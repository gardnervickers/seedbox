import java.util.concurrent.atomic.AtomicLong;

/**
 * Thousands of short platform threads: clone3, stack mapping and guard pages, TLS setup,
 * rseq registration and exit, in batches, with daemon threads exiting while others start.
 */
public final class ThreadChurn {
    public static void main(String[] args) throws Exception {
        Fz.init("ThreadChurn", args);
        AtomicLong total = new AtomicLong();
        int batches = 100 * Fz.scale, perBatch = 40;
        for (int b = 0; b < batches; b++) {
            Thread[] ts = new Thread[perBatch];
            for (int i = 0; i < perBatch; i++) {
                final long k = (long) b * perBatch + i;
                ts[i] = new Thread(() -> {
                    long h = k;
                    for (int j = 0; j < 200; j++) h = h * 6364136223846793005L + 1;
                    total.addAndGet(h & 0xffff);
                });
                ts[i].setDaemon((i & 3) == 0);
                ts[i].start();
            }
            Fz.join(ts);
        }
        Fz.out("threads", batches * perBatch);
        Fz.out("total", total.get());
        Fz.done();
    }
}
