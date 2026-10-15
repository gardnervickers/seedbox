import java.io.*;
import java.net.*;
import java.nio.ByteBuffer;
import java.nio.channels.*;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Iterator;

/** Loopback TCP with blocking sockets, a non-blocking NIO selector server, UDP datagrams and Unix-domain channels. */
public final class Sockets {
    static long checksum(byte[] b, int n) { long s = 0; for (int i = 0; i < n; i++) s = s * 31 + b[i]; return s; }

    public static void main(String[] args) throws Exception {
        Fz.init("Sockets", args);
        int clients = 2 + (int) (Fz.seed % 3), msgs = 200 * Fz.scale;
        long[] got = new long[clients];
        try (ServerSocket ss = new ServerSocket(0, 50, InetAddress.getLoopbackAddress())) {
            Thread server = new Thread(() -> {
                try {
                    for (int c = 0; c < clients; c++) {
                        Socket s = ss.accept();
                        new Thread(() -> {
                            try (s; DataInputStream in = new DataInputStream(new BufferedInputStream(s.getInputStream()));
                                 DataOutputStream out = new DataOutputStream(s.getOutputStream())) {
                                while (true) {
                                    int len = in.readInt();
                                    if (len < 0) break;
                                    byte[] b = in.readNBytes(len);
                                    out.writeLong(checksum(b, len));
                                    out.flush();
                                }
                            } catch (IOException e) { throw new UncheckedIOException(e); }
                        }).start();
                    }
                } catch (IOException e) { throw new UncheckedIOException(e); }
            });
            server.start();
            Thread[] ts = new Thread[clients];
            for (int c = 0; c < clients; c++) {
                final int id = c;
                ts[c] = new Thread(() -> {
                    java.util.SplittableRandom r = new java.util.SplittableRandom(Fz.seed + id);
                    try (Socket s = new Socket(InetAddress.getLoopbackAddress(), ss.getLocalPort())) {
                        s.setTcpNoDelay(true);
                        DataOutputStream out = new DataOutputStream(new BufferedOutputStream(s.getOutputStream()));
                        DataInputStream in = new DataInputStream(s.getInputStream());
                        for (int m = 0; m < msgs; m++) {
                            byte[] b = new byte[1 + r.nextInt(20_000)];
                            r.nextBytes(b);
                            out.writeInt(b.length);
                            out.write(b);
                            out.flush();
                            Fz.expect(in.readLong() == checksum(b, b.length), "echo checksum");
                            got[id] += b.length;
                        }
                        out.writeInt(-1);
                        out.flush();
                    } catch (IOException e) { throw new UncheckedIOException(e); }
                });
                ts[c].start();
            }
            Fz.join(ts);
            server.join();
        }
        Fz.out("tcp", java.util.Arrays.toString(got));
        long nio = 0;
        try (Selector sel = Selector.open(); ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(false);
            ssc.register(sel, SelectionKey.OP_ACCEPT);
            int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
            Thread[] cs = new Thread[clients];
            long[] echoed = new long[clients];
            for (int c = 0; c < clients; c++) {
                final int id = c;
                cs[c] = new Thread(() -> {
                    try (SocketChannel ch = SocketChannel.open(new InetSocketAddress(InetAddress.getLoopbackAddress(), port))) {
                        ByteBuffer w = ByteBuffer.allocate(4096), rd = ByteBuffer.allocate(4096);
                        for (int m = 0; m < msgs; m++) {
                            w.clear();
                            for (int i = 0; i < 1000; i++) w.put((byte) (m + i + id));
                            w.flip();
                            while (w.hasRemaining()) ch.write(w);
                            rd.clear();
                            while (rd.position() < 1000) ch.read(rd);
                            echoed[id] += rd.get(999);
                        }
                    } catch (IOException e) { throw new UncheckedIOException(e); }
                });
                cs[c].start();
            }
            int open = clients;
            int accepted = 0;
            ByteBuffer buf = ByteBuffer.allocate(8192);
            while (open > 0) {
                sel.select(1000);
                Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                while (it.hasNext()) {
                    SelectionKey k = it.next();
                    it.remove();
                    if (k.isAcceptable()) {
                        SocketChannel ch = ssc.accept();
                        if (ch != null) { ch.configureBlocking(false); ch.register(sel, SelectionKey.OP_READ); accepted++; }
                    } else if (k.isReadable()) {
                        SocketChannel ch = (SocketChannel) k.channel();
                        buf.clear();
                        int n = ch.read(buf);
                        if (n < 0) { ch.close(); open--; continue; }
                        buf.flip();
                        while (buf.hasRemaining()) ch.write(buf);
                    }
                }
            }
            Fz.join(cs);
            for (long e : echoed) nio += e;
            Fz.out("nioAccepted", accepted);
        }
        Fz.out("nio", nio);
        try (DatagramChannel a = DatagramChannel.open().bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
             DatagramChannel b = DatagramChannel.open().bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0))) {
            long sum = 0;
            ByteBuffer rb = ByteBuffer.allocate(2048);
            for (int i = 0; i < 500; i++) {
                a.send(ByteBuffer.wrap(("d" + i).getBytes()), b.getLocalAddress());
                rb.clear();
                b.receive(rb);
                rb.flip();
                sum += new String(rb.array(), 0, rb.limit()).hashCode();
            }
            Fz.out("udp", sum);
        }
        Path sock = Files.createTempDirectory("fzu").resolve("s");
        UnixDomainSocketAddress ua = UnixDomainSocketAddress.of(sock);
        try (ServerSocketChannel us = ServerSocketChannel.open(StandardProtocolFamily.UNIX).bind(ua)) {
            Thread t = new Thread(() -> {
                try (SocketChannel c = SocketChannel.open(ua)) {
                    for (int i = 0; i < 1000; i++) c.write(ByteBuffer.wrap(new byte[] {(byte) i}));
                } catch (IOException e) { throw new UncheckedIOException(e); }
            });
            t.start();
            long s = 0;
            try (SocketChannel c = us.accept()) {
                ByteBuffer b = ByteBuffer.allocate(256);
                int n;
                while ((n = c.read(b)) >= 0) { b.flip(); while (b.hasRemaining()) s += b.get(); b.clear(); }
            }
            t.join();
            Fz.out("unix", s);
        }
        Files.deleteIfExists(sock);
        Files.deleteIfExists(sock.getParent());
        Fz.done();
    }
}
