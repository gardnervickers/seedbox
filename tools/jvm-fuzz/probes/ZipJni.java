import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URI;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.Map;
import java.util.TreeMap;
import java.util.zip.Adler32;
import java.util.zip.CRC32;
import java.util.zip.CRC32C;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.GZIPInputStream;
import java.util.zip.GZIPOutputStream;
import java.util.zip.Inflater;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;

/**
 * JDK paths that cross JNI, written to be run also under -Xcheck:jni: zlib with preset
 * dictionaries (needsDictionary negotiation, raw streams, a wrong dictionary, direct and heap
 * ByteBuffers, flush modes, one-byte output windows and reset/reuse), checksums over arrays and
 * direct buffers, GZIP streams, a zip file system rewritten (move, delete, replace) and reopened
 * several times and then read through ZipFile and ZipInputStream, java.io file natives and
 * loopback TCP and UDP. Under -Xcheck:jni the output must not gain any JNI warning.
 */
public final class ZipJni {
    static byte[] text(int n) {
        String[] words = {"alpha", "beta", "gamma", "delta", "seedbox", "replay", "zlib", "dictionary", "\n"};
        StringBuilder b = new StringBuilder();
        while (b.length() < n) b.append(words[Fz.rnd.nextInt(words.length)]).append(' ');
        return b.substring(0, n).getBytes(StandardCharsets.US_ASCII);
    }

    /** Inflates with dictionary negotiation, through a deliberately small output window. */
    static byte[] inflate(byte[] packed, int n, byte[] dict, boolean nowrap, int window, boolean direct) throws DataFormatException {
        Inflater z = new Inflater(nowrap);
        try {
            if (nowrap && dict != null) z.setDictionary(dict);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            if (direct) {
                ByteBuffer in = ByteBuffer.allocateDirect(n).put(packed, 0, n).flip();
                z.setInput(in);
            } else {
                z.setInput(packed, 0, n);
            }
            byte[] buf = new byte[window];
            ByteBuffer dbuf = ByteBuffer.allocateDirect(window);
            int spins = 0;
            while (!z.finished()) {
                int k;
                if (direct) {
                    dbuf.clear();
                    k = z.inflate(dbuf);
                    dbuf.flip();
                    dbuf.get(buf, 0, k);
                } else {
                    k = z.inflate(buf);
                }
                out.write(buf, 0, k);
                if (k == 0 && z.needsDictionary()) {
                    Adler32 a = new Adler32();
                    a.update(dict);
                    Fz.expect(z.getAdler() == (int) a.getValue(), "dictionary id");
                    z.setDictionary(dict);
                }
                if (k == 0 && z.needsInput()) break;
                if (++spins > 10_000_000) Fz.expect(false, "inflate progress");
            }
            return out.toByteArray();
        } finally {
            z.end();
        }
    }

    static void dictionaries() throws Exception {
        long h = 0;
        int ok = 0, total = 0, wrongDict = 0;
        int[] flushes = {Deflater.NO_FLUSH, Deflater.SYNC_FLUSH, Deflater.FULL_FLUSH};
        Deflater reused = new Deflater();
        for (int r = 0; r < 60 * Fz.scale; r++) {
            byte[] dict = text(16 + Fz.rnd.nextInt(2000));
            byte[] data = text(1 + Fz.rnd.nextInt(20_000));
            boolean nowrap = Fz.rnd.nextInt(4) == 0, useDict = Fz.rnd.nextInt(5) != 0, direct = Fz.rnd.nextBoolean();
            int level = Fz.rnd.nextInt(-1, 10);
            Deflater d = nowrap ? new Deflater(level, true) : reused;
            if (!nowrap) {
                d.reset();
                d.setLevel(level);
            }
            d.setStrategy(Fz.rnd.nextInt(3) == 0 ? Deflater.FILTERED : Deflater.DEFAULT_STRATEGY);
            if (useDict) {
                if (direct) d.setDictionary(ByteBuffer.allocateDirect(dict.length).put(dict).flip());
                else d.setDictionary(dict);
            }
            int flush = flushes[Fz.rnd.nextInt(flushes.length)];
            byte[] packed = new byte[data.length + 1024];
            int n = 0;
            int half = data.length / 2;
            if (direct) d.setInput(ByteBuffer.allocateDirect(half).put(data, 0, half).flip());
            else d.setInput(data, 0, half);
            while (!d.needsInput()) n += d.deflate(packed, n, packed.length - n, flush);
            d.setInput(ByteBuffer.wrap(data, half, data.length - half));
            d.finish();
            while (!d.finished()) n += d.deflate(packed, n, packed.length - n);
            if (nowrap) d.end();
            byte[] back = inflate(packed, n, useDict ? dict : null, nowrap, 1 + Fz.rnd.nextInt(512), Fz.rnd.nextBoolean());
            total++;
            if (Arrays.equals(back, data)) ok++;
            h = h * 31 + n;
            Fz.mix(back);
            if (useDict && !nowrap && dict.length > 64) {
                byte[] wrong = dict.clone();
                wrong[0] ^= 1;
                try {
                    Inflater z = new Inflater();
                    z.setInput(packed, 0, n);
                    z.inflate(new byte[16]);
                    if (z.needsDictionary()) z.setDictionary(wrong);
                    z.end();
                } catch (IllegalArgumentException e) {
                    wrongDict++;
                }
            }
        }
        reused.end();
        Fz.out("dict.roundTrips", ok + "/" + total);
        Fz.out("dict.wrongRejected", wrongDict);
        Fz.out("dict.sizes", Long.toHexString(h));
        Fz.expect(ok == total, "dictionary round trips");
        Fz.mix(h);
    }

    static void checksums() {
        byte[] data = Fz.bytes(100_000);
        ByteBuffer direct = ByteBuffer.allocateDirect(data.length).put(data).flip();
        long h = 0;
        for (var c : new java.util.zip.Checksum[] {new CRC32(), new CRC32C(), new Adler32()}) {
            c.update(data, 3, data.length - 3);
            long a = c.getValue();
            c.reset();
            c.update(direct.duplicate().position(3));
            long b = c.getValue();
            c.reset();
            for (int off = 3; off < data.length; ) {
                int len = Math.min(data.length - off, 1 + Fz.rnd.nextInt(5000));
                if (Fz.rnd.nextBoolean()) c.update(data, off, len);
                else c.update(direct.duplicate().position(off).limit(off + len));
                off += len;
            }
            Fz.expect(a == b && b == c.getValue(), "checksum paths agree " + c.getClass().getSimpleName());
            h = h * 31 + a;
        }
        Fz.out("checksums", Long.toHexString(h));
        Fz.mix(h);
    }

    static void gzip() throws Exception {
        byte[] data = text(50_000 + Fz.rnd.nextInt(50_000));
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (GZIPOutputStream g = new GZIPOutputStream(bo, 512, true)) {
            for (int off = 0; off < data.length; ) {
                int len = Math.min(data.length - off, 1 + Fz.rnd.nextInt(3000));
                g.write(data, off, len);
                if (Fz.rnd.nextInt(8) == 0) g.flush();
                off += len;
            }
        }
        byte[] back;
        try (InputStream in = new GZIPInputStream(new ByteArrayInputStream(bo.toByteArray()), 333)) {
            back = in.readAllBytes();
        }
        Fz.out("gzip.roundTrip", Arrays.equals(back, data));
        Fz.mix(back);
    }

    static TreeMap<String, String> read(FileSystem z) throws Exception {
        TreeMap<String, String> m = new TreeMap<>();
        try (var s = Files.walk(z.getPath("/"))) {
            for (Path p : s.toList())
                if (Files.isRegularFile(p)) m.put(p.toString(), Integer.toHexString(Arrays.hashCode(Files.readAllBytes(p))));
        }
        return m;
    }

    static void zipfs() throws Exception {
        Path dir = Files.createTempDirectory("fz-zipfs");
        Path zip = dir.resolve("x.zip");
        URI uri = URI.create("jar:" + zip.toUri());
        Map<String, byte[]> model = new TreeMap<>();
        int rewrites = 4 + Fz.rnd.nextInt(3);
        for (int round = 0; round < rewrites; round++) {
            Map<String, Object> env = new HashMap<>();
            if (round == 0) env.put("create", "true");
            if (Fz.rnd.nextBoolean()) env.put("compressionMethod", "STORED");
            try (FileSystem z = FileSystems.newFileSystem(uri, env)) {
                int ops = 5 + Fz.rnd.nextInt(10);
                for (int k = 0; k < ops; k++) {
                    String name = "/d" + Fz.rnd.nextInt(3) + "/f" + Fz.rnd.nextInt(12);
                    Path p = z.getPath(name);
                    switch (Fz.rnd.nextInt(4)) {
                        case 0, 1 -> {
                            byte[] b = Fz.rnd.nextBoolean() ? text(1 + Fz.rnd.nextInt(30_000)) : Fz.bytes(Fz.rnd.nextInt(5000));
                            Files.createDirectories(p.getParent());
                            Files.write(p, b);
                            model.put(name, b);
                        }
                        case 2 -> {
                            if (model.containsKey(name)) {
                                String to = name + "m" + round;
                                Files.move(p, z.getPath(to));
                                model.put(to, model.remove(name));
                            }
                        }
                        default -> {
                            if (model.containsKey(name)) {
                                Files.delete(p);
                                model.remove(name);
                            }
                        }
                    }
                }
            }
            // Reopened read-only, the rewritten central directory holds exactly the model.
            try (FileSystem z = FileSystems.newFileSystem(uri, Map.of())) {
                TreeMap<String, String> got = read(z);
                TreeMap<String, String> want = new TreeMap<>();
                model.forEach((k, v) -> want.put(k, Integer.toHexString(Arrays.hashCode(v))));
                Fz.expect(got.equals(want), "zipfs contents after rewrite " + round);
            }
        }
        int entries = 0;
        long h = 0;
        try (ZipFile zf = new ZipFile(zip.toFile())) {
            for (Enumeration<? extends ZipEntry> en = zf.entries(); en.hasMoreElements(); ) {
                ZipEntry e = en.nextElement();
                if (e.isDirectory()) continue;
                try (InputStream in = zf.getInputStream(e)) {
                    byte[] b = in.readAllBytes();
                    Fz.expect(Arrays.equals(b, model.get("/" + e.getName())), "ZipFile entry " + e.getName());
                    h += Arrays.hashCode(b);
                }
                entries++;
            }
        }
        // ZipInputStream rejects STORED entries that zipfs wrote with a data descriptor; that
        // depends only on the seeded sequence, so the stop is part of the result.
        String streamed = "";
        int n = 0;
        try (ZipInputStream zin = new ZipInputStream(Files.newInputStream(zip))) {
            for (ZipEntry e; (e = zin.getNextEntry()) != null; ) {
                if (e.isDirectory()) continue;
                Fz.expect(Arrays.equals(zin.readAllBytes(), model.get("/" + e.getName())), "ZipInputStream entry");
                n++;
            }
            streamed = String.valueOf(n);
        } catch (java.util.zip.ZipException e) {
            streamed = n + "+ZipException";
        }
        Fz.out("zipfs.rewrites", rewrites);
        Fz.out("zipfs.entries", entries + " " + streamed + " " + model.size());
        Fz.out("zipfs.contentHash", Long.toHexString(h));
        Fz.mix(h);
        Files.delete(zip);
        Files.delete(dir);
    }

    static void ioNatives() throws Exception {
        File dir = Files.createTempDirectory("fz-jni").toFile();
        File f = new File(dir, "data.bin");
        byte[] data = Fz.bytes(70_000);
        try (OutputStream o = new FileOutputStream(f)) {
            for (int off = 0; off < data.length; ) {
                int len = Math.min(data.length - off, 1 + Fz.rnd.nextInt(9000));
                o.write(data, off, len);
                off += len;
            }
        }
        byte[] back = new byte[data.length];
        try (InputStream in = new FileInputStream(f)) {
            int off = 0;
            while (off < back.length) off += in.read(back, off, Math.min(back.length - off, 1 + Fz.rnd.nextInt(9000)));
        }
        long rafSum = 0;
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            for (int k = 0; k < 200; k++) {
                long pos = Fz.rnd.nextInt(data.length - 8);
                raf.seek(pos);
                rafSum += raf.readLong();
                raf.seek(pos);
                raf.writeInt(k);
            }
            raf.setLength(data.length / 2);
            rafSum += raf.length();
        }
        String[] names = dir.list();
        Fz.out("io.roundTrip", Arrays.equals(back, data) + " " + f.length() + " " + (names == null ? -1 : names.length));
        Fz.out("io.raf", Long.toHexString(rafSum));
        Fz.mix(rafSum);
        Fz.expect(f.delete() && dir.delete(), "delete");
    }

    static void loopback() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        byte[] msg = Fz.bytes(30_000);
        try (ServerSocket ss = new ServerSocket(0, 1, lo)) {
            Thread t = new Thread(() -> {
                try (Socket s = ss.accept()) {
                    byte[] in = s.getInputStream().readNBytes(msg.length);
                    for (int i = 0; i < in.length; i++) in[i] ^= 0x5a;
                    s.getOutputStream().write(in);
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
            t.start();
            byte[] echoed;
            try (Socket c = new Socket(lo, ss.getLocalPort())) {
                c.setTcpNoDelay(true);
                c.getOutputStream().write(msg);
                echoed = c.getInputStream().readNBytes(msg.length);
            }
            t.join();
            boolean same = echoed.length == msg.length;
            for (int i = 0; same && i < msg.length; i++) same = (byte) (msg[i] ^ 0x5a) == echoed[i];
            Fz.out("tcp.echo", same);
        }
        try (DatagramSocket a = new DatagramSocket(new InetSocketAddress(lo, 0));
             DatagramSocket b = new DatagramSocket(new InetSocketAddress(lo, 0))) {
            b.setSoTimeout(30_000);
            int got = 0;
            long h = 0;
            for (int k = 0; k < 20; k++) {
                byte[] p = Fz.bytes(1 + Fz.rnd.nextInt(1400));
                a.send(new DatagramPacket(p, p.length, b.getLocalSocketAddress()));
                DatagramPacket r = new DatagramPacket(new byte[2048], 2048);
                b.receive(r);
                if (r.getLength() == p.length && Arrays.equals(Arrays.copyOf(r.getData(), r.getLength()), p)) got++;
                h = h * 31 + r.getLength();
            }
            Fz.out("udp.received", got);
            Fz.mix(h);
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("ZipJni", args);
        dictionaries();
        checksums();
        gzip();
        zipfs();
        ioNatives();
        loopback();
        Fz.done();
    }
}
