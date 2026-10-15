import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Faults inside intrinsic memory accesses: a file is mapped, then truncated, so reading the
 * mapping past the new end raises SIGBUS. HotSpot's handler moves the PC to an error stub and
 * the access throws InternalError, from interpreted and compiled code alike.
 */
public final class MappedSigbus {
    static byte read(MappedByteBuffer b, int i) { return b.get(i); }

    public static void main(String[] args) throws Exception {
        Fz.init("MappedSigbus", args);
        Path file = Files.createTempFile("fz", ".map");
        int page = 4096;
        long okSum = 0;
        int faults = 0;
        try (FileChannel ch = FileChannel.open(file, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            byte[] data = new byte[4 * page];
            for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 7 + Fz.seed);
            ch.write(java.nio.ByteBuffer.wrap(data));
            MappedByteBuffer map = ch.map(FileChannel.MapMode.READ_ONLY, 0, 4 * page);
            ch.truncate(page);
            for (int i = 0; i < 20_000 * Fz.scale; i++) {
                int at = (i % 4 == 0) ? 2 * page + (i & 1023) : (i & 4095);
                try {
                    okSum += read(map, at);
                } catch (InternalError e) {
                    faults++;
                }
            }
        }
        Files.delete(file);
        Fz.out("faults", faults);
        Fz.out("okSum", okSum);
        Fz.done();
    }
}
