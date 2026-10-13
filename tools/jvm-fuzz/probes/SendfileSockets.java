import java.io.IOException;
import java.io.UncheckedIOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.zip.CRC32C;

/**
 * Kafka's fetch path: FileChannel.transferTo from a sparse file into loopback sockets (sendfile)
 * and transferFrom from sockets into the file, at positions crossing 2 GiB and with lengths around
 * 8 MiB, through small socket buffers so transfers make partial progress (non-blocking senders
 * wait in a selector). Meanwhile another thread appends records at the end of the same file and
 * forces it. Every byte has a model value given by its file position, so each received stream,
 * each region written by transferFrom and the appended records are checked against the model.
 */
public final class SendfileSockets {
    static final long G2 = 1L << 31, MB = 1L << 20;

    /** The model word holding file positions [8w, 8w+8); byte p is its byte p & 7. */
    static long modelWord(long seed, long w) {
        long x = w * 0x9E3779B97F4A7C15L ^ seed;
        x ^= x >>> 31;
        x *= 0xBF58476D1CE4E5B9L;
        return x ^ (x >>> 29);
    }

    /** Fills b's remaining bytes with the model bytes of positions pos, pos+1, ... */
    static void fillModel(long seed, long pos, ByteBuffer b) {
        int i = b.position(), lim = b.limit();
        long p = pos;
        for (; i < lim && (p & 7) != 0; i++, p++) b.put(i, (byte) (modelWord(seed, p >>> 3) >>> ((p & 7) * 8)));
        ByteOrder order = b.order();
        b.order(ByteOrder.LITTLE_ENDIAN);
        for (; lim - i >= 8; i += 8, p += 8) b.putLong(i, modelWord(seed, p >>> 3));
        b.order(order);
        for (; i < lim; i++, p++) b.put(i, (byte) (modelWord(seed, p >>> 3) >>> ((p & 7) * 8)));
    }

    static long modelCrc(long seed, long pos, long len) {
        CRC32C crc = new CRC32C();
        ByteBuffer b = ByteBuffer.allocate(1 << 16);
        for (long done = 0; done < len; ) {
            int n = (int) Math.min(b.capacity(), len - done);
            b.clear().limit(n);
            fillModel(seed, pos + done, b);
            crc.update(b);
            done += n;
        }
        return crc.getValue();
    }

    static void writeModel(FileChannel fc, long seed, long pos, long len) throws IOException {
        ByteBuffer b = ByteBuffer.allocateDirect(1 << 20);
        for (long done = 0; done < len; ) {
            int n = (int) Math.min(b.capacity(), len - done);
            b.clear().limit(n);
            fillModel(seed, pos + done, b);
            while (b.hasRemaining()) done += fc.write(b, pos + done);
        }
    }

    /** Reads exactly len bytes from s and returns their CRC. */
    static long drain(SocketChannel s, long len) throws IOException {
        CRC32C crc = new CRC32C();
        ByteBuffer b = ByteBuffer.allocateDirect(1 << 15);
        for (long got = 0; got < len; ) {
            b.clear().limit((int) Math.min(b.capacity(), len - got));
            int n = s.read(b);
            if (n < 0) throw new IOException("eof after " + got + " of " + len);
            b.flip();
            crc.update(b);
            got += n;
        }
        return crc.getValue();
    }

    static SocketChannel[] pair(ServerSocketChannel ss, int sndbuf) throws IOException {
        SocketChannel c = SocketChannel.open();
        c.setOption(StandardSocketOptions.SO_SNDBUF, sndbuf);
        c.connect(ss.getLocalAddress());
        SocketChannel s = ss.accept();
        return new SocketChannel[] {c, s};
    }

    public static void main(String[] args) throws Exception {
        Fz.init("SendfileSockets", args);
        Path f = Path.of(System.getProperty("java.io.tmpdir"), "fz-sendfile-" + ProcessHandle.current().pid() + ".dat");
        Files.deleteIfExists(f);
        long seed = Fz.seed;
        // Source data: 20 MiB starting 7 MiB (plus a seeded offset) below 2 GiB, and 64 KiB at 0.
        long srcBase = G2 - 7 * MB - (seed % 4096) * 3, srcLen = 20 * MB;
        long dstBase = G2 + 40 * MB + (seed % 977), end = G2 + 96 * MB;
        try (FileChannel fc = FileChannel.open(f, StandardOpenOption.CREATE_NEW, StandardOpenOption.READ,
                                               StandardOpenOption.WRITE, StandardOpenOption.SPARSE);
             ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.setOption(StandardSocketOptions.SO_RCVBUF, 4096);
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            writeModel(fc, seed, 0, 65536);
            writeModel(fc, seed, srcBase, srcLen);
            // The destination regions above 2 GiB stay holes until transferFrom writes them.
            fc.write(ByteBuffer.wrap(new byte[] {0}), end - 1);
            Fz.out("size", fc.size());

            // Records appended (and forced) at the end of the file while transfers run.
            int records = 600 * Fz.scale;
            Thread appender = new Thread(() -> {
                try (FileChannel ap = FileChannel.open(f, StandardOpenOption.WRITE, StandardOpenOption.APPEND)) {
                    ByteBuffer b = ByteBuffer.allocate(4096 + 64);
                    for (int r = 0; r < records; r++) {
                        int len = 64 + (int) ((seed * 31 + r * 977L) % 4096);
                        long at = end + recordOffset(seed, r);
                        b.clear().limit(len);
                        fillModel(seed, at, b);
                        while (b.hasRemaining()) ap.write(b);
                        if (r % 16 == 15) ap.force(r % 32 == 31);
                    }
                } catch (IOException e) {
                    throw new UncheckedIOException(e);
                }
            }, "fz-append");
            appender.start();

            long[][] shapes = {
                {srcBase + 7 * MB - 4096, 8192},              // crosses 2 GiB
                {srcBase, 8 * MB - 1}, {srcBase + 1, 8 * MB}, {srcBase + 3, 8 * MB + 1},
                {srcBase + 6 * MB, 9 * MB + 77},             // crosses 2 GiB and 8 MiB
                {srcBase + srcLen - 12345, 12345},            // ends at the end of the data
                {17, 65536 - 17},                             // low positions
                {srcBase + 7 * MB - 1, 1},                    // the last byte below 2 GiB
            };
            CRC32C all = new CRC32C();
            int rounds = shapes.length * Fz.scale;
            for (int k = 0; k < rounds; k++) {
                long[] sh = shapes[k % shapes.length];
                long pos = sh[0], len = sh[1];
                boolean nonBlocking = k % 2 == 1;
                SocketChannel[] pr = pair(ss, 4096 << (k % 3));
                long want = modelCrc(seed, pos, len);
                long[] got = {0};
                Thread recv = new Thread(() -> {
                    try { got[0] = drain(pr[1], len); } catch (IOException e) { throw new UncheckedIOException(e); }
                }, "fz-recv");
                recv.start();
                sendFile(fc, pos, len, pr[0], nonBlocking);
                recv.join();
                Fz.expect(got[0] == want, "transferTo " + k + " crc");
                all.update(longBytes(got[0]));
                pr[0].close();
                pr[1].close();

                // transferFrom: the model bytes of [pos, pos+len) arrive over a socket and land at
                // the same offset relative to dstBase; then the file region is checked.
                long dst = dstBase + (k % 4) * 9 * MB + (k * 4099L) % 8192;
                long tlen = Math.min(len, 9 * MB);
                SocketChannel[] pr2 = pair(ss, 4096 << (k % 3));
                Thread send = new Thread(() -> {
                    try {
                        ByteBuffer b = ByteBuffer.allocateDirect(1 << 16);
                        for (long done = 0; done < tlen; ) {
                            int n = (int) Math.min(b.capacity(), tlen - done);
                            b.clear().limit(n);
                            fillModel(seed, dst + done, b);
                            while (b.hasRemaining()) done += pr2[0].write(b);
                        }
                        pr2[0].shutdownOutput();
                    } catch (IOException e) {
                        throw new UncheckedIOException(e);
                    }
                }, "fz-send");
                send.start();
                long done = 0;
                while (done < tlen) {
                    long n = fc.transferFrom(pr2[1], dst + done, tlen - done);
                    if (n <= 0) break;
                    done += n;
                }
                send.join();
                Fz.expect(done == tlen, "transferFrom " + k + " moved " + done + " of " + tlen);
                Fz.expect(fileCrc(fc, dst, tlen) == modelCrc(seed, dst, tlen), "transferFrom " + k + " contents");
                pr2[0].close();
                pr2[1].close();
            }
            appender.join();
            long appended = recordOffset(seed, records);
            Fz.expect(fc.size() == end + appended, "appended size");
            Fz.expect(fileCrc(fc, end, appended) == modelCrc(seed, end, appended), "appended contents");
            // The source data is untouched.
            Fz.expect(fileCrc(fc, srcBase, srcLen) == modelCrc(seed, srcBase, srcLen), "source contents");
            Fz.out("rounds", rounds);
            Fz.out("appended", records + " records " + appended + " bytes");
            Fz.outHex("streams", all.getValue());
            Fz.mix(all.getValue());
            Fz.mix(appended);
        } finally {
            Files.deleteIfExists(f);
        }
        Fz.done();
    }

    static long recordOffset(long seed, int r) {
        long off = 0;
        for (int i = 0; i < r; i++) off += 64 + (int) ((seed * 31 + i * 977L) % 4096);
        return off;
    }

    static byte[] longBytes(long v) { return ByteBuffer.allocate(8).putLong(v).array(); }

    static long fileCrc(FileChannel fc, long pos, long len) throws IOException {
        CRC32C crc = new CRC32C();
        ByteBuffer b = ByteBuffer.allocateDirect(1 << 20);
        for (long done = 0; done < len; ) {
            b.clear().limit((int) Math.min(b.capacity(), len - done));
            int n = fc.read(b, pos + done);
            if (n <= 0) throw new IOException("short read at " + (pos + done));
            b.flip();
            crc.update(b);
            done += n;
        }
        return crc.getValue();
    }

    /** transferTo until len bytes are sent; a non-blocking socket waits for OP_WRITE between calls. */
    static void sendFile(FileChannel fc, long pos, long len, SocketChannel s, boolean nonBlocking) throws IOException {
        Selector sel = null;
        if (nonBlocking) {
            s.configureBlocking(false);
            sel = Selector.open();
            s.register(sel, SelectionKey.OP_WRITE);
        }
        try {
            for (long done = 0; done < len; ) {
                long n = fc.transferTo(pos + done, len - done, s);
                if (n < 0) throw new IOException("transferTo returned " + n);
                done += n;
                if (n == 0) {
                    if (sel != null) {
                        sel.select(1000);
                        sel.selectedKeys().clear();
                    } else {
                        Thread.onSpinWait();
                    }
                }
            }
        } finally {
            if (sel != null) sel.close();
        }
    }
}
