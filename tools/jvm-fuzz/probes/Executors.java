import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.Future;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.stream.IntStream;
import java.util.stream.LongStream;

/** Thread pools, scheduled tasks, fork/join, CompletableFuture graphs and parallel streams. */
public final class Executors {
    static final class Fib extends RecursiveTask<Long> {
        final int n;
        Fib(int n) { this.n = n; }
        protected Long compute() {
            if (n < 12) return slow(n);
            Fib a = new Fib(n - 1);
            a.fork();
            return new Fib(n - 2).compute() + a.join();
        }
        static long slow(int n) { return n < 2 ? n : slow(n - 1) + slow(n - 2); }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Executors", args);
        ExecutorService pool = java.util.concurrent.Executors.newFixedThreadPool(2 + (int) (Fz.seed % 3));
        List<Callable<Long>> tasks = new ArrayList<>();
        for (int i = 0; i < 64 * Fz.scale; i++) {
            final int k = i;
            tasks.add(() -> LongStream.range(0, 20_000 + k).map(x -> x * x % 1009).sum());
        }
        long s = 0;
        for (Future<Long> f : pool.invokeAll(tasks)) s += f.get();
        Fz.out("invokeAll", s);
        ScheduledExecutorService sched = java.util.concurrent.Executors.newScheduledThreadPool(2);
        List<ScheduledFuture<Integer>> sf = new ArrayList<>();
        for (int i = 0; i < 40; i++) {
            final int k = i;
            sf.add(sched.schedule(() -> k * k, (k * 7) % 13, TimeUnit.MILLISECONDS));
        }
        int ss = 0;
        for (ScheduledFuture<Integer> f : sf) ss += f.get();
        int[] ticks = {0};
        ScheduledFuture<?> rep = sched.scheduleAtFixedRate(() -> { synchronized (ticks) { ticks[0]++; } }, 0, 2, TimeUnit.MILLISECONDS);
        while (true) { synchronized (ticks) { if (ticks[0] >= 25) break; } Thread.sleep(1); }
        rep.cancel(false);
        Fz.out("scheduled", ss + " ticks>=25");
        ForkJoinPool fj = new ForkJoinPool(3);
        Fz.out("fib", fj.invoke(new Fib(24 + Fz.scale)));
        CompletableFuture<Long> cf = CompletableFuture.supplyAsync(() -> 21L, pool);
        for (int i = 0; i < 200; i++) {
            final int k = i;
            cf = cf.thenApplyAsync(x -> x * 3 % 1_000_003 + k, (k & 1) == 0 ? pool : fj)
                   .thenCombine(CompletableFuture.supplyAsync(() -> (long) k), Long::sum);
        }
        Fz.out("cf", cf.get());
        CompletableFuture<String> failed = CompletableFuture.supplyAsync(() -> { throw new IllegalStateException("boom"); }, pool)
            .thenApply(Object::toString).exceptionally(t -> t.getCause().getMessage());
        Fz.out("cfFail", failed.get());
        Fz.out("parallel", IntStream.range(0, 2_000_000 * Fz.scale).parallel().mapToLong(i -> (long) i * i % 97).sum());
        Fz.out("parallelSorted", IntStream.range(0, 100_000).parallel().map(i -> (i * 7919) % 100_003).sorted().limit(5).boxed().toList());
        pool.shutdown();
        sched.shutdown();
        fj.shutdown();
        Fz.expect(pool.awaitTermination(60, TimeUnit.SECONDS), "pool termination");
        Fz.done();
    }
}
