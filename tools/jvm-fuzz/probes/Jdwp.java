import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.Deque;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;

/**
 * A debugger in the same JVM, speaking JDWP over loopback to the JDWP agent (the runtime image has
 * no jdk.jdi), that rewrites the main thread's activations through JVMTI: forced early returns of
 * an int and of a debugger-created String at method entry, a local variable changed at a
 * breakpoint, a frame popped and re-executed, and line single-stepping. Run with
 * -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=127.0.0.1:38629,quiet=y
 */
public final class Jdwp {
    static final int PORT = 38629;

    /** Debuggee methods, one statement per line so the line table separates them. */
    static final class Target {
        static int counter;

        static int f(int x) {
            return x + 1;
        }

        static String s(int x) {
            return "real" + x;
        }

        static int g(int x) {
            int y = x * 2;
            return y + 1;
        }

        static int h(int x) {
            counter++;
            return x * 100 + counter;
        }

        static int k(int n) {
            int s = 0;
            for (int i = 0; i < n; i++)
                s += i;
            return s;
        }
    }

    /** One JDWP connection: synchronous commands, events queued while a reply is awaited. */
    static final class Wire {
        final DataInputStream in;
        final DataOutputStream out;
        final Deque<DataInputStream> events = new ArrayDeque<>();
        int nextId = 1, idSize = 8;

        Wire(Socket s) throws IOException {
            s.setTcpNoDelay(true);
            in = new DataInputStream(new java.io.BufferedInputStream(s.getInputStream()));
            out = new DataOutputStream(new java.io.BufferedOutputStream(s.getOutputStream()));
            byte[] hs = "JDWP-Handshake".getBytes(StandardCharsets.US_ASCII);
            out.write(hs);
            out.flush();
            byte[] back = new byte[hs.length];
            in.readFully(back);
            if (!new String(back, StandardCharsets.US_ASCII).equals("JDWP-Handshake")) throw new IOException("handshake");
        }

        DataInputStream command(int set, int cmd, Body body) throws IOException {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            DataOutputStream d = new DataOutputStream(b);
            if (body != null) body.write(d);
            int id = nextId++;
            out.writeInt(11 + b.size());
            out.writeInt(id);
            out.writeByte(0);
            out.writeByte(set);
            out.writeByte(cmd);
            out.write(b.toByteArray());
            out.flush();
            while (true) {
                int len = in.readInt(), pid = in.readInt(), flags = in.readUnsignedByte();
                byte[] rest = new byte[len - 9];
                in.readFully(rest);
                DataInputStream p = new DataInputStream(new java.io.ByteArrayInputStream(rest));
                if ((flags & 0x80) != 0) {
                    int error = p.readUnsignedShort();
                    if (pid != id) throw new IOException("reply for " + pid);
                    if (error != 0) throw new IOException("JDWP error " + error + " for " + set + "/" + cmd);
                    return p;
                }
                int s = p.readUnsignedByte(), c = p.readUnsignedByte();
                if (s == 64 && c == 100) events.add(p);
            }
        }

        DataInputStream event() throws IOException {
            if (!events.isEmpty()) return events.poll();
            while (true) {
                int len = in.readInt();
                in.readInt();
                int flags = in.readUnsignedByte();
                byte[] rest = new byte[len - 9];
                in.readFully(rest);
                DataInputStream p = new DataInputStream(new java.io.ByteArrayInputStream(rest));
                if ((flags & 0x80) != 0) throw new IOException("unexpected reply");
                if (p.readUnsignedByte() == 64 && p.readUnsignedByte() == 100) return p;
            }
        }

        long id(DataInputStream d) throws IOException { return idSize == 8 ? d.readLong() : d.readInt() & 0xffffffffL; }

        void id(DataOutputStream d, long v) throws IOException {
            if (idSize == 8) d.writeLong(v);
            else d.writeInt((int) v);
        }
    }

    interface Body {
        void write(DataOutputStream d) throws IOException;
    }

    /** A decoded single event: kind, request, thread, location method and code index. */
    record Ev(int kind, int request, long thread, long method, long index) {}

    static Ev next(Wire w) throws IOException {
        DataInputStream p = w.event();
        p.readUnsignedByte();
        int n = p.readInt();
        if (n != 1) throw new IOException("composite of " + n);
        int kind = p.readUnsignedByte(), req = p.readInt();
        if (kind == 90) return new Ev(kind, req, w.id(p), 0, 0);
        if (kind != 1 && kind != 2 && kind != 40) throw new IOException("event kind " + kind);
        long thread = w.id(p);
        p.readUnsignedByte();
        w.id(p);
        long method = w.id(p);
        long index = p.readLong();
        return new Ev(kind, req, thread, method, index);
    }

    static Ev nextOf(Wire w, int kind) throws IOException {
        while (true) {
            Ev e = next(w);
            if (e.kind == kind) return e;
        }
    }

    static int[] vals, xs;
    static int nf, ns, ng, nh, kn;
    static final CountDownLatch attached = new CountDownLatch(1), aDone = new CountDownLatch(1), aCleared = new CountDownLatch(1),
        bDone = new CountDownLatch(1), bCleared = new CountDownLatch(1), finished = new CountDownLatch(1);
    static final Semaphore armed = new Semaphore(0), returned = new Semaphore(0);
    static volatile String failure;
    static volatile int stepsInK = -1;
    static String forcedString;

    static void await(CountDownLatch l, String what) throws InterruptedException {
        Fz.expect(l.await(120, TimeUnit.SECONDS) && failure == null, what + (failure == null ? "" : ": " + failure));
    }

    /**
     * The JDWP socket transport gives a connected debugger two seconds of wall time to send its
     * handshake, so the client path runs once against a local echo server before the real connect.
     */
    static void warmUp() throws Exception {
        try (java.net.ServerSocket server = new java.net.ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress())) {
            Thread echo = new Thread(() -> {
                try (Socket c = server.accept()) {
                    c.getOutputStream().write(c.getInputStream().readNBytes(14));
                } catch (IOException e) {
                    failure = "warm-up: " + e;
                }
            });
            echo.start();
            try (Socket sock = new Socket()) {
                sock.connect(server.getLocalSocketAddress(), 5_000);
                new Wire(sock);
            }
            echo.join();
        }
    }

    static void debugger() {
        try (Socket sock = new Socket()) {
            warmUp();
            for (int i = 0; ; i++) {
                try {
                    sock.connect(new InetSocketAddress("127.0.0.1", PORT), 5_000);
                    break;
                } catch (IOException e) {
                    if (i > 200) throw e;
                    Thread.sleep(10);
                }
            }
            Wire w = new Wire(sock);
            DataInputStream r = w.command(1, 7, null);
            int fieldId = r.readInt(), methodId = r.readInt(), objectId = r.readInt(), refId = r.readInt(), frameId = r.readInt();
            if (fieldId != 8 || methodId != 8 || objectId != 8 || refId != 8 || frameId != 8) throw new IOException("id sizes");
            r = w.command(1, 2, d -> writeString(d, "LJdwp$Target;"));
            if (r.readInt() != 1) throw new IOException("Target not loaded");
            r.readUnsignedByte();
            long type = w.id(r);
            r = w.command(2, 5, d -> w.id(d, type));
            int methods = r.readInt();
            long mf = 0, ms = 0, mg = 0, mh = 0, mk = 0;
            for (int i = 0; i < methods; i++) {
                long id = w.id(r);
                String name = readString(r);
                readString(r);
                r.readInt();
                switch (name) {
                    case "f" -> mf = id;
                    case "s" -> ms = id;
                    case "g" -> mg = id;
                    case "h" -> mh = id;
                    case "k" -> mk = id;
                    default -> { }
                }
            }

            // Forced early returns at method entry.
            int entry = request(w, 40, 1, d -> { d.writeByte(4); w.id(d, type); });
            r = w.command(1, 11, d -> writeString(d, forcedString));
            long str = w.id(r);
            w.command(9, 7, d -> w.id(d, str));
            attached.countDown();
            int fi = 0;
            for (int handled = 0; handled < nf + ns; handled++) {
                Ev e = nextOf(w, 40);
                if (e.method == mf) {
                    int v = vals[fi++];
                    w.command(11, 14, d -> { w.id(d, e.thread); d.writeByte('I'); d.writeInt(v); });
                } else if (e.method == ms) {
                    w.command(11, 14, d -> { w.id(d, e.thread); d.writeByte('s'); w.id(d, str); });
                }
                w.command(11, 3, d -> w.id(d, e.thread));
            }
            aDone.await();
            w.command(15, 2, d -> { d.writeByte(40); d.writeInt(entry); });

            // A local changed at a breakpoint on g's second line.
            long gLine = line(w, type, mg, 1);
            long gm = mg;
            int bp = request(w, 2, 1, d -> { d.writeByte(7); location(w, d, type, gm, gLine); });
            aCleared.countDown();
            for (int i = 0; i < ng; i++) {
                Ev e = nextOf(w, 2);
                long frame = topFrame(w, e.thread);
                r = w.command(16, 1, d -> {
                    w.id(d, e.thread); w.id(d, frame); d.writeInt(2);
                    d.writeInt(0); d.writeByte('I'); d.writeInt(1); d.writeByte('I');
                });
                r.readInt();
                r.readUnsignedByte();
                int x = r.readInt();
                r.readUnsignedByte();
                int y = r.readInt();
                if (y != x * 2) throw new IOException("local y=" + y + " x=" + x);
                w.command(16, 2, d -> { w.id(d, e.thread); w.id(d, frame); d.writeInt(1); d.writeInt(1); d.writeByte('I'); d.writeInt(y + 1000 + x); });
                w.command(11, 3, d -> w.id(d, e.thread));
            }
            bDone.await();
            w.command(15, 2, d -> { d.writeByte(2); d.writeInt(bp); });
            bCleared.countDown();

            // Pop h's frame after its side effect, once per call; h runs again.
            long hLine = line(w, type, mh, 1);
            long hm = mh;
            for (int round = 0; round < nh; round++) {
                request(w, 2, 2, d -> { d.writeByte(1); d.writeInt(1); d.writeByte(7); location(w, d, type, hm, hLine); });
                armed.release();
                Ev e = nextOf(w, 2);
                long frame = topFrame(w, e.thread);
                w.command(16, 4, d -> { w.id(d, e.thread); w.id(d, frame); });
                w.command(11, 3, d -> w.id(d, e.thread));
                returned.acquire();
            }

            // Line steps over k from its first line until control is back in the caller.
            long kLine = line(w, type, mk, 0);
            long km = mk;
            request(w, 2, 2, d -> { d.writeByte(1); d.writeInt(1); d.writeByte(7); location(w, d, type, km, kLine); });
            armed.release();
            Ev e = nextOf(w, 2);
            int step = request(w, 1, 1, d -> { d.writeByte(10); w.id(d, e.thread); d.writeInt(1); d.writeInt(1); });
            w.command(11, 3, d -> w.id(d, e.thread));
            int steps = 0;
            while (true) {
                Ev s = nextOf(w, 1);
                if (s.method != km) {
                    w.command(15, 2, d -> { d.writeByte(1); d.writeInt(step); });
                    w.command(11, 3, d -> w.id(d, s.thread));
                    break;
                }
                steps++;
                w.command(11, 3, d -> w.id(d, s.thread));
            }
            stepsInK = steps;
            w.command(1, 6, null);
        } catch (Throwable t) {
            failure = t.toString();
            attached.countDown();
            aCleared.countDown();
            bCleared.countDown();
            armed.release(100);
        } finally {
            finished.countDown();
        }
    }

    /** EventRequest.Set with suspend policy EVENT_THREAD and `count` modifiers. */
    static int request(Wire w, int kind, int count, Body modifiers) throws IOException {
        DataInputStream r = w.command(15, 1, d -> {
            d.writeByte(kind);
            d.writeByte(1);
            d.writeInt(count);
            modifiers.write(d);
        });
        return r.readInt();
    }

    static void location(Wire w, DataOutputStream d, long type, long method, long index) throws IOException {
        d.writeByte(1);
        w.id(d, type);
        w.id(d, method);
        d.writeLong(index);
    }

    /** The code index where the `nth` distinct source line of `method` starts. */
    static long line(Wire w, long type, long method, int nth) throws IOException {
        DataInputStream r = w.command(6, 1, d -> { w.id(d, type); w.id(d, method); });
        r.readLong();
        r.readLong();
        int n = r.readInt();
        long[][] lines = new long[n][];
        for (int i = 0; i < n; i++) lines[i] = new long[] {r.readLong(), r.readInt()};
        java.util.Arrays.sort(lines, (a, b) -> Long.compare(a[0], b[0]));
        if (nth >= n) throw new IOException("line table of " + n);
        return lines[nth][0];
    }

    static long topFrame(Wire w, long thread) throws IOException {
        DataInputStream r = w.command(11, 6, d -> { w.id(d, thread); d.writeInt(0); d.writeInt(1); });
        if (r.readInt() != 1) throw new IOException("frames");
        return w.id(r);
    }

    static void writeString(DataOutputStream d, String s) throws IOException {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        d.writeInt(b.length);
        d.write(b);
    }

    static String readString(DataInputStream d) throws IOException {
        byte[] b = new byte[d.readInt()];
        d.readFully(b);
        return new String(b, StandardCharsets.UTF_8);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Jdwp", args);
        nf = 20 + Fz.rnd.nextInt(20);
        ns = 5 + Fz.rnd.nextInt(5);
        ng = 10 + Fz.rnd.nextInt(10);
        nh = 5 + Fz.rnd.nextInt(5);
        kn = 3 + Fz.rnd.nextInt(6);
        vals = new int[nf];
        for (int i = 0; i < nf; i++) vals[i] = Fz.rnd.nextInt();
        xs = new int[ng];
        for (int i = 0; i < ng; i++) xs[i] = Fz.rnd.nextInt(1 << 20);
        forcedString = "forced" + Fz.seed;
        Target.counter = 0;
        Thread d = new Thread(Jdwp::debugger, "fz-jdwp-debugger");
        d.setDaemon(true);
        d.start();
        await(attached, "debugger attached (run with -agentlib:jdwp=...address=127.0.0.1:" + PORT + ")");

        int forced = 0, forcedStrings = 0;
        for (int i = 0; i < nf; i++) if (Target.f(i) == vals[i]) forced++;
        for (int i = 0; i < ns; i++) if (Target.s(i).equals(forcedString)) forcedStrings++;
        aDone.countDown();
        await(aCleared, "entry request cleared, breakpoint set");
        Fz.out("forcedEarlyReturn", forced + "/" + nf + " strings=" + forcedStrings + "/" + ns + " after=" + Target.f(41));

        int localsSet = 0;
        for (int i = 0; i < ng; i++) if (Target.g(xs[i]) == xs[i] * 2 + 1000 + xs[i] + 1) localsSet++;
        bDone.countDown();
        await(bCleared, "breakpoint cleared");
        Fz.out("localSetAtBreakpoint", localsSet + "/" + ng + " after=" + Target.g(5));

        int reran = 0;
        for (int r = 0; r < nh; r++) {
            Fz.expect(armed.tryAcquire(120, TimeUnit.SECONDS) && failure == null, "pop request armed " + failure);
            int before = Target.counter;
            int v = Target.h(r);
            if (Target.counter == before + 2 && v == r * 100 + before + 2) reran++;
            returned.release();
        }
        Fz.out("poppedFrames", reran + "/" + nh + " counter=" + Target.counter);

        Fz.expect(armed.tryAcquire(120, TimeUnit.SECONDS) && failure == null, "step request armed " + failure);
        int kv = Target.k(kn);
        await(finished, "debugger finished");
        Fz.out("singleStep", "k=" + kv + " steps=" + stepsInK);
        Fz.expect(failure == null, "debugger: " + failure);
        Fz.expect(forced == nf && forcedStrings == ns && localsSet == ng && reran == nh && stepsInK > kn, "debugger effects");
        Fz.mix(stepsInK);
        Fz.mix(Target.counter);
        Fz.done();
    }
}
