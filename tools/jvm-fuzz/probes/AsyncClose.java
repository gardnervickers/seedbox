import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousCloseException;
import java.nio.channels.ClosedByInterruptException;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.Pipe;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.TreeMap;

/**
 * Closing or interrupting a thread blocked in I/O. The JDK wakes the blocked thread with a
 * signal (NativeThread.signal), so the syscall is interrupted from another thread. Also writes
 * to a socket whose peer closed (SIGPIPE). Outcomes are printed by category.
 */
public final class AsyncClose {
    interface Blocker { void block() throws Exception; }

    static String category(Throwable t) {
        if (t == null) return "returned";
        if (t instanceof ClosedByInterruptException) return "interrupted";
        if (t instanceof AsynchronousCloseException || t instanceof ClosedChannelException) return "closed";
        if (t instanceof java.net.SocketException) return "socketClosed";
        return t.getClass().getSimpleName();
    }

    static String round(Blocker blocker, Runnable wake) throws Exception {
        Throwable[] result = {null};
        boolean[] started = {false};
        Thread t = new Thread(() -> {
            synchronized (started) { started[0] = true; started.notifyAll(); }
            try { blocker.block(); } catch (Throwable e) { result[0] = e; }
        });
        t.start();
        synchronized (started) { while (!started[0]) started.wait(); }
        // Give the thread time to enter the blocking syscall before waking it.
        while (t.getState() == Thread.State.NEW) Thread.onSpinWait();
        Thread.sleep(20);
        wake.run();
        t.join(60_000);
        Fz.expect(!t.isAlive(), "blocked thread did not wake");
        return category(result[0]);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("AsyncClose", args);
        TreeMap<String, Integer> outcomes = new TreeMap<>();
        InetAddress lo = InetAddress.getLoopbackAddress();
        for (int r = 0; r < 10 * Fz.scale; r++) {
            ServerSocketChannel ssc = ServerSocketChannel.open().bind(new InetSocketAddress(lo, 0));
            outcomes.merge("accept-close " + round(ssc::accept, () -> close(ssc)), 1, Integer::sum);

            ServerSocketChannel srv = ServerSocketChannel.open().bind(new InetSocketAddress(lo, 0));
            SocketChannel a = SocketChannel.open(srv.getLocalAddress());
            SocketChannel b = srv.accept();
            outcomes.merge("read-close " + round(() -> a.read(ByteBuffer.allocate(16)), () -> close(a)), 1, Integer::sum);
            b.close();
            srv.close();

            ServerSocketChannel srv2 = ServerSocketChannel.open().bind(new InetSocketAddress(lo, 0));
            SocketChannel c = SocketChannel.open(srv2.getLocalAddress());
            SocketChannel d = srv2.accept();
            Thread[] reader = {null};
            outcomes.merge("read-interrupt " + round(() -> { reader[0] = Thread.currentThread(); c.read(ByteBuffer.allocate(16)); },
                () -> reader[0].interrupt()), 1, Integer::sum);
            d.close();
            srv2.close();

            Pipe pipe = Pipe.open();
            outcomes.merge("pipe-close " + round(() -> pipe.source().read(ByteBuffer.allocate(16)), () -> close(pipe.source())), 1, Integer::sum);
            pipe.sink().close();

            ServerSocket ss = new ServerSocket(0, 5, lo);
            Socket s1 = new Socket(lo, ss.getLocalPort());
            Socket s2 = ss.accept();
            InputStream in = s1.getInputStream();
            outcomes.merge("socket-close " + round(in::read, () -> close(s1)), 1, Integer::sum);
            s2.close();
            ss.close();
        }
        int broken = 0;
        for (int r = 0; r < 20 * Fz.scale; r++) {
            ServerSocket ss = new ServerSocket(0, 5, lo);
            Socket s1 = new Socket(lo, ss.getLocalPort());
            Socket s2 = ss.accept();
            s2.close();
            OutputStream out = s1.getOutputStream();
            try {
                for (int i = 0; i < 10_000; i++) out.write(new byte[4096]);
            } catch (IOException e) {
                broken++;
            }
            s1.close();
            ss.close();
        }
        outcomes.forEach((k, v) -> Fz.out(k, v));
        Fz.out("brokenPipe", broken);
        Fz.done();
    }

    static void close(java.io.Closeable c) {
        try { c.close(); } catch (IOException e) { throw new RuntimeException(e); }
    }
}
