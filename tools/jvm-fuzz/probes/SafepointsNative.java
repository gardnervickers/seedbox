import java.io.RandomAccessFile;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.zip.CRC32;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

/**
 * VM operations while workers sit in native code (zlib, CRC32, file reads): safepoints proceed
 * without them, and their native-to-Java transitions must block correctly. Also checks that a
 * spinning thread's CPU time advances. Results are fixed by the work, not the schedule.
 */
public final class SafepointsNative {
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Fz.init("SafepointsNative", args);
        AtomicBoolean done = new AtomicBoolean();
        byte[] data = Fz.bytes(1 << 20);
        Path file = Files.createTempFile("fz", ".bin");
        Files.write(file, data);
        long[] results = new long[4];
        Thread[] ts = new Thread[4];
        ts[0] = new Thread(() -> {
            long h = 0;
            for (int r = 0; r < 60 * Fz.scale; r++) {
                CRC32 crc = new CRC32();
                crc.update(data);
                h = h * 31 + crc.getValue();
            }
            results[0] = h;
        });
        ts[1] = new Thread(() -> {
            long h = 0;
            byte[] out = new byte[data.length * 2], back = new byte[data.length];
            for (int r = 0; r < 6 * Fz.scale; r++) {
                Deflater d = new Deflater(r % 9);
                d.setInput(data);
                d.finish();
                int n = d.deflate(out);
                d.end();
                Inflater inf = new Inflater();
                inf.setInput(out, 0, n);
                try { inf.inflate(back); } catch (Exception e) { throw new RuntimeException(e); }
                inf.end();
                h = h * 31 + n + (java.util.Arrays.equals(back, data) ? 1 : 0);
            }
            results[1] = h;
        });
        ts[2] = new Thread(() -> {
            long h = 0;
            try (FileChannel ch = FileChannel.open(file)) {
                ByteBuffer b = ByteBuffer.allocateDirect(4096);
                for (int r = 0; r < 4000 * Fz.scale; r++) {
                    b.clear();
                    ch.read(b, (r * 4096L) % data.length);
                    h = h * 31 + b.get(r & 4095);
                }
            } catch (Exception e) { throw new RuntimeException(e); }
            results[2] = h;
        });
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ts[3] = new Thread(() -> {
            long h = 1;
            while (!done.get()) for (int i = 0; i < 100_000; i++) h = h * 6364136223846793005L + 1;
            sink = h;
        }, "fz-spinner");
        for (Thread t : ts) t.start();
        int ops = 0;
        while (ts[0].isAlive() || ts[1].isAlive() || ts[2].isAlive()) {
            System.gc();
            Thread.getAllStackTraces();
            mx.dumpAllThreads(true, true);
            ops++;
            Thread.sleep(5);
        }
        long spinCpu = mx.getThreadCpuTime(ts[3].getId());
        done.set(true);
        Fz.join(ts);
        Files.delete(file);
        Fz.expect(ops > 0, "at least one VM operation round");
        for (int i = 0; i < 3; i++) Fz.out("worker" + i, results[i]);
        Fz.out("spinnerCpuAdvanced", spinCpu > 1_000_000L);
        Fz.done();
    }
}
