import java.lang.reflect.Field;
import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import sun.misc.Unsafe;

/**
 * Translator memory-map bookkeeping under churn: large direct buffers freed by their
 * Cleaners, file mappings created and dropped, and big Unsafe allocations, with reads that
 * verify contents.
 */
public final class MmapChurn {
    public static void main(String[] args) throws Exception {
        Fz.init("MmapChurn", args);
        long sum = 0;
        for (int r = 0; r < 60 * Fz.scale; r++) {
            ByteBuffer b = ByteBuffer.allocateDirect((1 << 20) + r * 4096);
            b.putLong(r * 8, r);
            sum += b.getLong(r * 8);
            if (r % 10 == 9) System.gc();
        }
        Path file = Files.createTempFile("fz", ".churn");
        try (FileChannel ch = FileChannel.open(file, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            ch.write(ByteBuffer.wrap(new byte[1 << 20]));
            for (int r = 0; r < 300 * Fz.scale; r++) {
                MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_WRITE, (r % 16) * 4096L, 64 * 1024);
                m.putInt(0, r);
                sum += m.getInt(0);
                if (r % 50 == 49) System.gc();
            }
        }
        Files.delete(file);
        Field f = Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Unsafe u = (Unsafe) f.get(null);
        for (int r = 0; r < 20 * Fz.scale; r++) {
            long size = 16L << 20;
            long base = u.allocateMemory(size);
            u.setMemory(base, size, (byte) r);
            sum += u.getByte(base + size - 1) + u.getByte(base);
            u.freeMemory(base);
        }
        Fz.out("sum", sum);
        Fz.done();
    }
}
