import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.CompletionHandler;
import java.nio.channels.FileLock;
import java.nio.file.ClosedWatchServiceException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.nio.file.StandardWatchEventKinds;
import java.nio.file.WatchEvent;
import java.nio.file.WatchKey;
import java.nio.file.WatchService;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumSet;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * File notification and asynchronous file I/O: inotify-backed WatchService registration, event
 * decoding, key reset and cancellation, two watched directories, a timed poll that expires, and
 * a closed service; then AsynchronousFileChannel positional writes and reads through futures and
 * completion handlers, on the default pool and on a supplied executor, with locks, truncation and
 * force. Every wait is for a specific event or completion, so the printed results never depend
 * on timing.
 */
public final class AsyncFiles {
    static WatchKey take(WatchService w) throws InterruptedException {
        WatchKey k = w.poll(30, TimeUnit.SECONDS);
        Fz.expect(k != null, "watch event within 30 s");
        return k;
    }

    /** Collects events for one key until it holds the expected kind for the expected name. */
    static int awaitEvent(WatchService w, WatchKey key, WatchEvent.Kind<?> kind, Path name) throws InterruptedException {
        int seen = 0;
        for (int tries = 0; tries < 50; tries++) {
            WatchKey k = take(w);
            Fz.expect(k == key, "event for the expected key");
            boolean hit = false;
            for (WatchEvent<?> e : k.pollEvents()) {
                Fz.expect(e.kind() != StandardWatchEventKinds.OVERFLOW, "no overflow");
                if (e.kind() == kind && name.equals(e.context())) hit = true;
                seen++;
            }
            k.reset();
            if (hit) return seen;
        }
        Fz.expect(false, "event " + kind + " " + name);
        return seen;
    }

    static void watch() throws Exception {
        Path a = Files.createTempDirectory("fz-watch-a"), b = Files.createTempDirectory("fz-watch-b");
        int rounds = 40 + Fz.rnd.nextInt(40) * Fz.scale;
        int creates = 0, deletes = 0, modifies = 0;
        long names = 0;
        try (WatchService w = a.getFileSystem().newWatchService()) {
            WatchKey ka = a.register(w, StandardWatchEventKinds.ENTRY_CREATE, StandardWatchEventKinds.ENTRY_DELETE,
                StandardWatchEventKinds.ENTRY_MODIFY);
            WatchKey kb = b.register(w, StandardWatchEventKinds.ENTRY_CREATE);
            Fz.out("watch.sameKeyOnReregister", a.register(w, StandardWatchEventKinds.ENTRY_CREATE,
                StandardWatchEventKinds.ENTRY_DELETE, StandardWatchEventKinds.ENTRY_MODIFY) == ka);
            for (int r = 0; r < rounds; r++) {
                boolean inB = Fz.rnd.nextInt(4) == 0;
                Path dir = inB ? b : a;
                WatchKey key = inB ? kb : ka;
                Path name = Path.of("f" + Fz.rnd.nextInt(1_000_000));
                Path p = dir.resolve(name);
                Files.write(p, Fz.bytes(1 + Fz.rnd.nextInt(64)));
                awaitEvent(w, key, StandardWatchEventKinds.ENTRY_CREATE, name);
                creates++;
                names += name.toString().hashCode();
                if (!inB) {
                    // A modification of an existing file, then its deletion.
                    Files.write(p, Fz.bytes(8), StandardOpenOption.APPEND);
                    awaitEvent(w, key, StandardWatchEventKinds.ENTRY_MODIFY, name);
                    modifies++;
                    Files.delete(p);
                    awaitEvent(w, key, StandardWatchEventKinds.ENTRY_DELETE, name);
                    deletes++;
                } else {
                    Files.delete(p);
                }
            }
            // Nothing pending: a timed poll expires.
            Fz.out("watch.idlePoll", w.poll(20, TimeUnit.MILLISECONDS) == null);
            kb.cancel();
            Fz.out("watch.cancelledValid", kb.isValid());
            Files.write(b.resolve("after-cancel"), new byte[1]);
            Files.write(a.resolve("marker"), new byte[1]);
            awaitEvent(w, ka, StandardWatchEventKinds.ENTRY_CREATE, Path.of("marker"));
            Fz.out("watch.validAfter", ka.isValid());
            w.close();
            String closed;
            try {
                w.poll();
                closed = "none";
            } catch (ClosedWatchServiceException e) {
                closed = e.getClass().getSimpleName();
            }
            Fz.out("watch.closed", closed + " " + ka.isValid());
        }
        Fz.out("watch.events", creates + " " + modifies + " " + deletes);
        Fz.mix(names);
        Fz.mix(creates * 1_000_000L + modifies * 1000L + deletes);
        for (Path d : new Path[] {a, b}) {
            try (var s = Files.list(d)) {
                for (Path p : s.toList()) Files.delete(p);
            }
            Files.delete(d);
        }
    }

    static void async() throws Exception {
        Path p = Files.createTempFile("fz-async", ".bin");
        int size = 1 << 16;
        byte[] model = new byte[size];
        ExecutorService pool = Executors.newFixedThreadPool(2);
        for (int pass = 0; pass < 2; pass++) {
            Arrays.fill(model, (byte) 0);
            AsynchronousFileChannel c = pass == 0
                ? AsynchronousFileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING)
                : AsynchronousFileChannel.open(p, EnumSet.of(StandardOpenOption.READ, StandardOpenOption.WRITE,
                    StandardOpenOption.TRUNCATE_EXISTING), pool);
            try (c) {
                // Many outstanding positional writes to disjoint regions, by future and handler.
                List<Future<Integer>> futures = new ArrayList<>();
                int ops = 64 * Fz.scale;
                CountDownLatch handlers = new CountDownLatch(ops);
                AtomicLong handled = new AtomicLong();
                AtomicInteger failed = new AtomicInteger();
                int region = size / (2 * ops);
                for (int i = 0; i < 2 * ops; i++) {
                    int len = 1 + Fz.rnd.nextInt(region);
                    long pos = (long) i * region;
                    byte[] data = Fz.bytes(len);
                    System.arraycopy(data, 0, model, (int) pos, len);
                    ByteBuffer buf = Fz.rnd.nextBoolean() ? ByteBuffer.wrap(data) : ByteBuffer.allocateDirect(len).put(data).flip();
                    if ((i & 1) == 0) futures.add(c.write(buf, pos));
                    else c.write(buf, pos, len, new CompletionHandler<Integer, Integer>() {
                        @Override public void completed(Integer n, Integer want) {
                            if (n.intValue() != want) failed.incrementAndGet();
                            handled.addAndGet(n);
                            handlers.countDown();
                        }
                        @Override public void failed(Throwable t, Integer want) {
                            failed.incrementAndGet();
                            handlers.countDown();
                        }
                    });
                }
                long written = 0;
                for (Future<Integer> f : futures) written += f.get(30, TimeUnit.SECONDS);
                Fz.expect(handlers.await(30, TimeUnit.SECONDS), "completion handlers ran");
                c.force(true);
                long fileSize = c.size();
                // Read back the whole file in unaligned pieces.
                byte[] back = new byte[(int) fileSize];
                int off = 0;
                while (off < back.length) {
                    int len = Math.min(back.length - off, 1 + Fz.rnd.nextInt(4096));
                    ByteBuffer rb = ByteBuffer.allocate(len);
                    int n = c.read(rb, off).get(30, TimeUnit.SECONDS);
                    Fz.expect(n > 0, "positional read");
                    System.arraycopy(rb.array(), 0, back, off, n);
                    off += n;
                }
                boolean same = Arrays.equals(back, Arrays.copyOf(model, back.length));
                // Reading past the end reports -1 through the future and the handler.
                int eof = c.read(ByteBuffer.allocate(8), fileSize + 100).get();
                CountDownLatch eofDone = new CountDownLatch(1);
                int[] eofHandler = {0};
                c.read(ByteBuffer.allocate(8), fileSize + 1, null, new CompletionHandler<Integer, Void>() {
                    @Override public void completed(Integer n, Void a) { eofHandler[0] = n; eofDone.countDown(); }
                    @Override public void failed(Throwable t, Void a) { eofHandler[0] = -99; eofDone.countDown(); }
                });
                eofDone.await(30, TimeUnit.SECONDS);
                // An exclusive region lock, a shared lock elsewhere, truncation.
                FileLock lock = c.lock(0, 4096, false).get();
                FileLock shared = c.tryLock(8192, 4096, true);
                boolean locks = lock.isValid() && !lock.isShared() && shared != null && shared.isShared();
                lock.release();
                shared.release();
                long cut = fileSize / 2 + Fz.rnd.nextInt(100);
                c.truncate(cut);
                String key = "async." + (pass == 0 ? "default" : "executor");
                Fz.out(key + ".written", written + " " + handled.get() + " " + failed.get());
                Fz.out(key + ".size", fileSize + " " + c.size());
                Fz.out(key + ".readBack", same);
                Fz.out(key + ".eof", eof + " " + eofHandler[0]);
                Fz.out(key + ".locks", locks);
                Fz.mix(back);
                Fz.mix(written + handled.get());
            }
            String closed;
            try {
                c.read(ByteBuffer.allocate(1), 0).get();
                closed = "none";
            } catch (Exception e) {
                closed = e.getClass().getSimpleName();
            }
            Fz.out("async.afterClose", closed);
        }
        pool.shutdown();
        Fz.expect(pool.awaitTermination(30, TimeUnit.SECONDS), "pool shutdown");
        Files.delete(p);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("AsyncFiles", args);
        watch();
        async();
        Fz.done();
    }
}
