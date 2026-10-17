import java.io.*;
import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.file.*;
import java.util.Comparator;
import java.util.stream.Stream;

/** Files through streams, RandomAccessFile, positional channels, memory mapping, locks and directory walks. */
public final class FileIo {
    public static void main(String[] args) throws Exception {
        Fz.init("FileIo", args);
        Path dir = Files.createTempDirectory("fz");
        int files = 20 * Fz.scale;
        for (int i = 0; i < files; i++) {
            Path sub = dir.resolve("d" + (i % 4));
            Files.createDirectories(sub);
            byte[] b = Fz.bytes(1000 + i * 37);
            if (i % 3 == 0) Files.write(sub.resolve("f" + i), b);
            else if (i % 3 == 1) try (OutputStream o = new BufferedOutputStream(new FileOutputStream(sub.resolve("f" + i).toFile()))) { o.write(b); }
            else try (FileChannel ch = FileChannel.open(sub.resolve("f" + i), StandardOpenOption.CREATE, StandardOpenOption.WRITE)) {
                ch.write(ByteBuffer.wrap(b));
                ch.force(true);
            }
        }
        long walk;
        try (Stream<Path> s = Files.walk(dir)) {
            walk = s.filter(Files::isRegularFile).sorted().mapToLong(p -> {
                try { return p.getFileName().toString().hashCode() * 31L + Files.size(p) + java.util.Arrays.hashCode(Files.readAllBytes(p)); }
                catch (IOException e) { throw new UncheckedIOException(e); }
            }).reduce(17, (a, b) -> a * 31 + b);
        }
        Fz.out("walk", walk);
        File raf = dir.resolve("raf").toFile();
        try (RandomAccessFile r = new RandomAccessFile(raf, "rw")) {
            r.setLength((1 << 16) + 8);
            for (int i = 0; i < 2000; i++) {
                r.seek(Fz.rnd.nextInt(1 << 16));
                r.writeLong(i);
            }
            r.seek(0);
            long s = 0;
            for (int i = 0; i < 1 << 13; i++) s += r.readLong();
            Fz.out("raf", s + " " + r.length());
        }
        Path mapped = dir.resolve("mapped");
        int size = 8 << 20;
        try (FileChannel ch = FileChannel.open(mapped, StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            MappedByteBuffer mb = ch.map(FileChannel.MapMode.READ_WRITE, 0, size);
            for (int i = 0; i < 200_000 * Fz.scale; i++) mb.putLong(Fz.rnd.nextInt(size / 8) * 8, i);
            mb.force();
            try (FileLock lock = ch.lock(0, 4096, false)) { Fz.out("lock", lock.isValid() && !lock.isShared()); }
            ByteBuffer rd = ByteBuffer.allocate(1 << 16);
            long s = 0;
            for (long pos = 0; pos < size; pos += rd.capacity()) {
                rd.clear();
                ch.read(rd, pos);
                rd.flip();
                while (rd.hasRemaining()) s = s * 31 + rd.getLong();
            }
            Fz.outHex("mapped", s);
            try (FileChannel out = FileChannel.open(dir.resolve("copy"), StandardOpenOption.CREATE, StandardOpenOption.WRITE)) {
                Fz.out("transfer", ch.transferTo(0, size, out));
            }
        }
        Files.move(dir.resolve("copy"), dir.resolve("moved"), StandardCopyOption.ATOMIC_MOVE);
        Fz.out("moved", Files.mismatch(mapped, dir.resolve("moved")));
        try (Stream<Path> s = Files.walk(dir)) {
            for (Path p : s.sorted(Comparator.reverseOrder()).toList()) Files.delete(p);
        }
        Fz.out("cleaned", !Files.exists(dir));
        Fz.done();
    }
}
