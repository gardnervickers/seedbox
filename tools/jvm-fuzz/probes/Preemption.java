/**
 * CPU-bound threads must be preempted: a sleeping or busy main thread has to get the single CPU back
 * while spinners run. Without preemption the first part never finishes and the second counts zero.
 */
public final class Preemption {
    static volatile boolean stop;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Fz.init("Preemption", args);
        int spinners = 1 + (int) (Fz.seed % 3);
        Thread[] ts = new Thread[spinners];
        for (int i = 0; i < spinners; i++) {
            ts[i] = new Thread(() -> {
                long n = 0;
                while (!stop) n++;
                sink += n;
            });
            ts[i].start();
        }
        Thread.sleep(20);
        stop = true;
        Fz.join(ts);
        Fz.out("sleeperWoke", true);
        Thread worker = new Thread(() -> {
            long h = 1;
            for (int i = 0; i < 200_000_000 * Fz.scale; i++) { h ^= h << 13; h ^= h >>> 7; h ^= h << 17; }
            sink = h;
        });
        worker.start();
        long mainTurns = 0;
        while (worker.isAlive()) mainTurns++;
        Fz.out("mainRanDuringWorker", mainTurns > 0);
        Fz.done();
    }
}
