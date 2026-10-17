import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.security.MessageDigest;

/**
 * A Kafka-shaped write path: many appends each followed by fdatasync, segment rolls by rename
 * with directory fsync, then a large mapped write forced to disk and read back by hash.
 * Exercises writeback, the block device model and dirty tracking.
 */
public final class Durability {
    public static void main(String[] args) throws Exception {
        Fz.init("Durability", args);
        Path dir = Files.createTempDirectory("fz-log");
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        int segment = 0;
        Path active = dir.resolve("active.log");
        FileChannel log = FileChannel.open(active, StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.APPEND);
        for (int i = 0; i < 2000 * Fz.scale; i++) {
            byte[] rec = Fz.bytes(64 + (i % 1000));
            md.update(rec);
            log.write(ByteBuffer.wrap(rec));
            log.force(false);
            if (i % 250 == 249) {
                log.close();
                Files.move(active, dir.resolve(String.format("%08d.log", segment++)), StandardCopyOption.ATOMIC_MOVE);
                try (FileChannel d = FileChannel.open(dir, StandardOpenOption.READ)) { d.force(true); }
                log = FileChannel.open(active, StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.APPEND);
            }
        }
        log.close();
        MessageDigest back = MessageDigest.getInstance("SHA-256");
        for (int s = 0; s < segment; s++) back.update(Files.readAllBytes(dir.resolve(String.format("%08d.log", s))));
        back.update(Files.readAllBytes(active));
        Fz.out("logIntact", MessageDigest.isEqual(md.digest(), back.digest()));
        Fz.out("segments", segment);
        Path big = dir.resolve("mapped.bin");
        long size = 256L << 20;
        long h = 0;
        try (FileChannel ch = FileChannel.open(big, StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            for (long off = 0; off < size; off += 64L << 20) {
                MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_WRITE, off, 64L << 20);
                for (int i = 0; i < m.capacity(); i += 4096) m.putLong(i, off + i);
                m.force();
            }
            ByteBuffer b = ByteBuffer.allocate(8);
            for (long off = 0; off < size; off += 1 << 20) {
                b.clear();
                ch.read(b, off);
                h = h * 31 + b.getLong(0);
            }
        }
        Fz.outHex("mapped", h);
        try (var files = Files.list(dir)) { files.forEach(p -> p.toFile().delete()); }
        Files.delete(dir);
        Fz.done();
    }
}
