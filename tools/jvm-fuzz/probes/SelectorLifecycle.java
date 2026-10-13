import java.io.IOException;
import java.io.UncheckedIOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.ClosedSelectorException;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.BitSet;
import java.util.List;
import java.util.SplittableRandom;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * One selector over 200 loopback connections, as a Kafka network thread uses it: a wakeup before
 * select must be consumed by exactly one select; four threads coalesce wakeups while the selector
 * thread mixes select(1), selectNow, select(100) and select(); keys are cancelled and channels
 * registered again under a new generation tag, so a ready key must always carry its channel's
 * current tag and cancelled registrations never show up; data sent to a channel while it is not
 * registered must appear once it is registered again. Finally a channel is closed while another
 * thread is blocked selecting, its descriptor is reused by a new connection, and a selector is
 * closed under a blocked select. Ready sets are compared as sets; iteration order is ignored.
 */
public final class SelectorLifecycle {
    record Tag(int conn, int gen) {}

    static final int N = 200;
    static SocketChannel[] client = new SocketChannel[N], server = new SocketChannel[N];
    static SelectionKey[] keys = new SelectionKey[N];
    static int[] gen = new int[N];
    static long[] pendingBytes = new long[N];   // sent, not yet read
    static long[] crc = new long[N];             // running hash of what each connection read
    static int staleKeys;

    static byte msgByte(long seed, int conn, long off) {
        long x = (off >>> 3) * 0x9E3779B97F4A7C15L ^ seed ^ (long) conn << 40;
        x ^= x >>> 29;
        x *= 0xBF58476D1CE4E5B9L;
        return (byte) (x >>> ((off & 7) * 8));
    }

    static long[] sentBytes = new long[N];

    static void send(long seed, int conn, int len) throws IOException {
        ByteBuffer b = ByteBuffer.allocate(len);
        for (int i = 0; i < len; i++) b.put(msgByte(seed, conn, sentBytes[conn] + i));
        b.flip();
        while (b.hasRemaining()) client[conn].write(b);
        sentBytes[conn] += len;
        pendingBytes[conn] += len;
    }

    static long[] readBytes = new long[N];

    /** Reads what a ready key's channel has, checking every byte against the model. */
    static void consume(long seed, SelectionKey k, BitSet ready, ByteBuffer buf) throws IOException {
        Tag t = (Tag) k.attachment();
        if (!k.isValid() || t.gen() != gen[t.conn()] || keys[t.conn()] != k) {
            staleKeys++;
            return;
        }
        int c = t.conn();
        ready.set(c);
        while (true) {
            buf.clear();
            int n = server[c].read(buf);
            if (n <= 0) break;
            for (int i = 0; i < n; i++) {
                if (buf.get(i) != msgByte(seed, c, readBytes[c] + i)) Fz.expect(false, "conn " + c + " byte " + (readBytes[c] + i));
                crc[c] = crc[c] * 31 + buf.get(i);
            }
            readBytes[c] += n;
            pendingBytes[c] -= n;
        }
    }

    static int selectOnce(Selector sel, int kind) throws IOException {
        return switch (kind) {
            case 0 -> sel.select(1);
            case 1 -> sel.selectNow();
            case 2 -> sel.select(100);
            default -> sel.select();
        };
    }

    public static void main(String[] args) throws Exception {
        Fz.init("SelectorLifecycle", args);
        long seed = Fz.seed;
        SplittableRandom r = new SplittableRandom(seed);
        Selector sel = Selector.open();
        ServerSocketChannel ss = ServerSocketChannel.open();
        ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), N);
        for (int i = 0; i < N; i++) {
            client[i] = SocketChannel.open(ss.getLocalAddress());
            client[i].socket().setTcpNoDelay(true);
            server[i] = ss.accept();
            server[i].configureBlocking(false);
            keys[i] = server[i].register(sel, SelectionKey.OP_READ, new Tag(i, 0));
        }

        // A wakeup before select makes the next select return at once, and only that one.
        sel.wakeup();
        long t0 = System.nanoTime();
        int n0 = sel.select(60_000);
        Fz.expect(n0 == 0 && sel.selectedKeys().isEmpty(), "select after wakeup");
        Fz.expect(System.nanoTime() - t0 < 30_000_000_000L, "pre-wakeup returned");
        long t1 = System.nanoTime();
        Fz.expect(sel.select(100) == 0, "second select");
        Fz.expect(System.nanoTime() - t1 >= 50_000_000L, "wakeup consumed by the first select");
        sel.wakeup();
        Fz.expect(sel.selectNow() == 0, "selectNow after wakeup");
        long t2 = System.nanoTime();
        sel.select(100);
        Fz.expect(System.nanoTime() - t2 >= 50_000_000L, "selectNow consumed the wakeup");

        ByteBuffer buf = ByteBuffer.allocate(1 << 14);
        int rounds = 120 * Fz.scale;
        long readySizes = 0, reregistered = 0, deferred = 0;
        BitSet off = new BitSet(N);   // cancelled and not yet registered again
        AtomicBoolean wakers = new AtomicBoolean(true);
        Thread[] ws = new Thread[4];
        for (int w = 0; w < ws.length; w++) {
            ws[w] = new Thread(() -> {
                while (wakers.get()) {
                    sel.wakeup();
                    for (int i = 0; i < 200; i++) Thread.onSpinWait();
                    Thread.yield();
                }
            }, "fz-waker-" + w);
        }
        for (int round = 0; round < rounds; round++) {
            if (round == rounds / 3) for (Thread t : ws) t.start();
            if (round == 2 * rounds / 3) {
                wakers.set(false);
                for (Thread t : ws) t.join();
            }
            // Cancel a few registrations; their channels may still receive data.
            int cancels = r.nextInt(6);
            for (int k = 0; k < cancels; k++) {
                int c = r.nextInt(N);
                if (off.get(c)) continue;
                keys[c].cancel();
                off.set(c);
            }
            // Messages to a seeded subset; channels that are off are expected later.
            int m = 1 + r.nextInt(40);
            BitSet expect = new BitSet(N);
            for (int k = 0; k < m; k++) {
                int c = r.nextInt(N);
                send(seed, c, 1 + r.nextInt(3000));
                if (!off.get(c)) expect.set(c);
                else deferred++;
            }
            // Bring some cancelled channels back under a new generation; their backlog is expected.
            if (round % 3 == 2) {
                sel.selectNow();   // deregisters the cancelled keys
                sel.selectedKeys().clear();
                for (int c = off.nextSetBit(0); c >= 0; c = off.nextSetBit(c + 1)) {
                    gen[c]++;
                    keys[c] = server[c].register(sel, SelectionKey.OP_READ, new Tag(c, gen[c]));
                    reregistered++;
                    if (pendingBytes[c] > 0) expect.set(c);
                }
                off.clear();
            }
            BitSet ready = new BitSet(N);
            for (int spins = 0; ; spins++) {
                boolean outstanding = false;
                for (int c = expect.nextSetBit(0); c >= 0; c = expect.nextSetBit(c + 1)) outstanding |= pendingBytes[c] > 0;
                if (!outstanding) break;
                Fz.expect(spins < 1_000_000, "round " + round + " never drained");
                selectOnce(sel, r.nextInt(4));
                for (SelectionKey k : sel.selectedKeys()) consume(seed, k, ready, buf);
                sel.selectedKeys().clear();
            }
            BitSet extra = (BitSet) ready.clone();
            extra.andNot(expect);
            Fz.expect(extra.isEmpty(), "round " + round + " unexpected ready " + extra);
            readySizes += ready.cardinality();
            Fz.expect(ready.equals(expect), "round " + round + " ready " + ready + " want " + expect);
        }
        Fz.expect(staleKeys == 0, staleKeys + " stale keys");

        // Close a registered channel while another thread is blocked in select, reuse its
        // descriptor for a new connection, and require the new key, never the old one.
        int victim = r.nextInt(N);
        if (off.get(victim)) {
            sel.selectNow();
            gen[victim]++;
            keys[victim] = server[victim].register(sel, SelectionKey.OP_READ, new Tag(victim, gen[victim]));
            off.clear(victim);
        }
        AtomicBoolean selecting = new AtomicBoolean(true);
        List<Integer> seen = new ArrayList<>();
        Thread blocked = new Thread(() -> {
            try {
                while (selecting.get()) {
                    sel.select();
                    synchronized (seen) {
                        for (SelectionKey k : sel.selectedKeys()) seen.add(((Tag) k.attachment()).conn());
                    }
                    sel.selectedKeys().clear();
                }
            } catch (IOException e) {
                throw new UncheckedIOException(e);
            }
        }, "fz-blocked");
        blocked.start();
        Thread.sleep(20);
        SelectionKey oldKey = keys[victim];
        server[victim].close();
        client[victim].close();
        selecting.set(false);
        sel.wakeup();
        blocked.join();
        sel.selectNow();
        sel.selectedKeys().clear();
        Fz.expect(!oldKey.isValid(), "closed channel's key is invalid");
        // Nothing was readable while the other thread selected: the closed channel never shows.
        Fz.expect(seen.isEmpty(), "keys selected while blocked: " + seen);
        client[victim] = SocketChannel.open(ss.getLocalAddress());
        server[victim] = ss.accept();
        server[victim].configureBlocking(false);
        gen[victim]++;
        sentBytes[victim] = readBytes[victim] = pendingBytes[victim] = 0;
        keys[victim] = server[victim].register(sel, SelectionKey.OP_READ, new Tag(victim, gen[victim]));
        send(seed ^ 1, victim, 777);
        BitSet ready = new BitSet(N);
        while (pendingBytes[victim] > 0) {
            sel.select(100);
            for (SelectionKey k : sel.selectedKeys()) consume(seed ^ 1, k, ready, buf);
            sel.selectedKeys().clear();
        }
        Fz.expect(staleKeys == 0 && ready.cardinality() == 1 && ready.get(victim), "reused descriptor " + ready);

        // A selector closed while another thread is blocked in it.
        Selector sel2 = Selector.open();
        SocketChannel quiet = SocketChannel.open(ss.getLocalAddress());
        SocketChannel quietPeer = ss.accept();
        quietPeer.configureBlocking(false);
        quietPeer.register(sel2, SelectionKey.OP_READ);
        String[] outcome = {"none"};
        Thread inSel2 = new Thread(() -> {
            try {
                sel2.select();
                try {
                    sel2.select(1);
                    outcome[0] = "open";
                } catch (ClosedSelectorException e) {
                    outcome[0] = "closed";
                }
            } catch (IOException e) {
                outcome[0] = "io";
            } catch (ClosedSelectorException e) {
                outcome[0] = "closed";
            }
        }, "fz-sel2");
        inSel2.start();
        Thread.sleep(20);
        sel2.close();
        inSel2.join();
        Fz.expect(outcome[0].equals("closed"), "blocked select on a closed selector: " + outcome[0]);
        quiet.close();
        quietPeer.close();

        long h = 0;
        for (int c = 0; c < N; c++) h = h * 0x100000001B3L ^ crc[c] ^ readBytes[c] << 20;
        Fz.out("rounds", rounds);
        Fz.out("readyTotal", readySizes);
        Fz.out("reregistered", reregistered);
        Fz.out("deferredMessages", deferred);
        Fz.outHex("data", h);
        Fz.out("sel2", outcome[0]);
        Fz.mix(h);
        Fz.mix(readySizes);
        for (int c = 0; c < N; c++) {
            client[c].close();
            server[c].close();
        }
        sel.close();
        ss.close();
        Fz.done();
    }
}
