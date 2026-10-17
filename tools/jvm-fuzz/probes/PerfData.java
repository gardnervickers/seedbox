import java.nio.ByteOrder;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;

/**
 * HotSpot's performance-data file (run with -XX:+UsePerfData): the JVM's own shared mapping of
 * /tmp/hsperfdata_USER/PID, mapped a second time here and parsed like jstat does. Collector
 * invocation, live-thread and sampled counters must move as the program acts, seen through the
 * second mapping. Only deltas that the program determines and booleans are printed.
 */
public final class PerfData {
    record Entry(char type, int vectorLength, int dataOffset) {}

    static MappedByteBuffer buf;
    static Map<String, Entry> entries = new LinkedHashMap<>();

    static void parse() {
        entries.clear();
        int entryOffset = buf.getInt(24), numEntries = buf.getInt(28);
        int off = entryOffset;
        for (int i = 0; i < numEntries; i++) {
            int len = buf.getInt(off), nameOff = buf.getInt(off + 4), vlen = buf.getInt(off + 8);
            char type = (char) buf.get(off + 12);
            int dataOff = buf.getInt(off + 16);
            StringBuilder name = new StringBuilder();
            for (int p = off + nameOff; buf.get(p) != 0; p++) name.append((char) buf.get(p));
            entries.put(name.toString(), new Entry(type, vlen, off + dataOff));
            off += len;
        }
    }

    static long counter(String name) {
        Entry e = entries.get(name);
        Fz.expect(e != null && e.type == 'J' && e.vectorLength == 0, "long counter " + name);
        return buf.getLong(e.dataOffset);
    }

    static String string(String name) {
        Entry e = entries.get(name);
        Fz.expect(e != null && e.type == 'B', "string counter " + name);
        byte[] b = new byte[e.vectorLength];
        buf.get(e.dataOffset, b);
        int n = 0;
        while (n < b.length && b[n] != 0) n++;
        return new String(b, 0, n, StandardCharsets.UTF_8);
    }

    static long collections() {
        long sum = 0;
        for (String name : entries.keySet()) {
            if (name.matches("sun\\.gc\\.collector\\.\\d+\\.invocations")) sum += counter(name);
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("PerfData", args);
        long pid = ProcessHandle.current().pid();
        Path file = Path.of("/tmp", "hsperfdata_" + System.getProperty("user.name"), Long.toString(pid));
        Fz.out("fileExists", Files.exists(file));
        Fz.expect(Files.exists(file), "perfdata file (run with -XX:+UsePerfData)");
        try (FileChannel ch = FileChannel.open(file, StandardOpenOption.READ)) {
            buf = ch.map(FileChannel.MapMode.READ_ONLY, 0, ch.size());
        }
        int magic = buf.order(ByteOrder.BIG_ENDIAN).getInt(0);
        buf.order(buf.get(4) == 0 ? ByteOrder.BIG_ENDIAN : ByteOrder.LITTLE_ENDIAN);
        Fz.out("prologue", Integer.toHexString(magic) + " v" + buf.get(5) + "." + buf.get(6) + " accessible=" + buf.get(7));
        parse();
        Fz.out("manyEntries", entries.size() > 100);
        Fz.out("vmNameMatches", string("java.property.java.vm.name").equals(System.getProperty("java.vm.name")));
        Fz.out("versionMatches", string("java.property.java.version").equals(System.getProperty("java.version")));
        Fz.out("commandMatches", string("sun.rt.javaCommand").startsWith("PerfData "));
        Fz.out("seedInCommand", string("sun.rt.javaCommand").endsWith(" " + Fz.seed + " " + Fz.scale));

        int gcs = 3 + Fz.rnd.nextInt(4);
        long before = collections();
        for (int i = 0; i < gcs; i++) System.gc();
        parse();
        long delta = collections() - before;
        Fz.out("gcs", gcs + " counted=" + (delta >= gcs));

        int threads = 2 + Fz.rnd.nextInt(5);
        long live = counter("java.threads.live"), started = counter("java.threads.started");
        CountDownLatch up = new CountDownLatch(threads), stop = new CountDownLatch(1);
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(() -> {
                up.countDown();
                try { stop.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
            });
            ts[i].start();
        }
        up.await();
        long liveDuring = counter("java.threads.live") - live, startedDuring = counter("java.threads.started") - started;
        stop.countDown();
        Fz.join(ts);
        long liveAfter = 0;
        for (int i = 0; i < 1000 && (liveAfter = counter("java.threads.live") - live) != 0; i++) Thread.sleep(1);
        Fz.out("threads", threads + " live=" + liveDuring + " started=" + startedDuring + " after=" + liveAfter);
        Fz.expect(liveDuring == threads && startedDuring == threads && liveAfter == 0, "thread counters");

        long loaded = counter("java.cls.loadedClasses");
        Class.forName("PerfData$Late");
        Fz.out("classLoadCounted", counter("java.cls.loadedClasses") > loaded);

        // The StatSampler task updates sampled counters every PerfDataSamplingInterval (50 ms).
        long ticks = counter("sun.os.hrt.ticks");
        boolean sampled = false;
        for (int i = 0; i < 200 && !sampled; i++) {
            Thread.sleep(25);
            sampled = counter("sun.os.hrt.ticks") != ticks;
        }
        Fz.out("sampled", sampled);
        Fz.expect(sampled, "sampler updates the shared mapping");
        Fz.mix(gcs);
        Fz.mix(threads);
        Fz.done();
    }

    static final class Late {}
}
