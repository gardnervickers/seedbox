import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.channels.Selector;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;
import sun.misc.Signal;

/**
 * Signals that interrupt threads in timed blocking syscalls (nanosleep, futex, epoll_wait,
 * poll on a socket). The kernel restarts them with a recomputed timeout; each wait must still
 * last its full timeout and end normally. Requires --enable-preview.
 */
public final class InterruptedSyscalls {
    interface Wait { void run() throws Exception; }

    public static void main(String[] args) throws Exception {
        Fz.init("InterruptedSyscalls", args);
        Semaphore handled = new Semaphore(0);
        Signal.handle(new Signal("HUP"), s -> handled.release());
        ServerSocket server = new ServerSocket(0, 50, InetAddress.getLoopbackAddress());
        Socket client = new Socket(InetAddress.getLoopbackAddress(), server.getLocalPort());
        Socket peer = server.accept();
        client.setSoTimeout(150);
        Selector selector = Selector.open();
        Object monitor = new Object();
        String[] names = {"sleep", "parkNanos", "select", "wait", "socketRead"};
        Wait[] waits = {
            () -> Thread.sleep(150),
            () -> { long end = System.nanoTime() + 150_000_000L; long left; while ((left = end - System.nanoTime()) > 0) LockSupport.parkNanos(left); },
            () -> { long end = System.nanoTime() + 150_000_000L; long left; while ((left = (end - System.nanoTime()) / 1_000_000) > 0) selector.select(left); },
            () -> { long end = System.nanoTime() + 150_000_000L; long left; synchronized (monitor) { while ((left = (end - System.nanoTime()) / 1_000_000) > 0) monitor.wait(left); } },
            () -> { try { client.getInputStream().read(); throw new IllegalStateException("read returned"); } catch (SocketTimeoutException e) { } },
        };
        int rounds = 4 * Fz.scale;
        boolean[] longEnough = new boolean[waits.length];
        java.util.Arrays.fill(longEnough, true);
        int signalled = 0;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < waits.length; k++) {
                final int kind = k;
                long[] elapsed = {0};
                Throwable[] failure = {null};
                java.util.concurrent.CountDownLatch finished = new java.util.concurrent.CountDownLatch(1);
                java.util.concurrent.CountDownLatch release = new java.util.concurrent.CountDownLatch(1);
                Thread t = new Thread(() -> {
                    long t0 = System.nanoTime();
                    try { waits[kind].run(); } catch (Throwable e) { failure[0] = e; }
                    elapsed[0] = System.nanoTime() - t0;
                    finished.countDown();
                    // Stay alive until signalling stops: a signal sent to an exiting thread is lost.
                    try { release.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
                }, "is-" + names[k]);
                t.start();
                int tid = Native.tid("is-" + names[k]);
                while (finished.getCount() > 0) {
                    if (Native.tgkill(tid, Native.SIGHUP)) {
                        Fz.expect(handled.tryAcquire(30, TimeUnit.SECONDS), "signal not handled");
                        signalled++;
                    }
                    Thread.sleep(5);
                }
                release.countDown();
                t.join();
                Fz.expect(failure[0] == null, names[k] + " failed: " + failure[0]);
                longEnough[k] &= elapsed[0] >= 149_000_000L;
            }
        }
        for (int k = 0; k < waits.length; k++) Fz.out(names[k] + "LastedTimeout", longEnough[k]);
        Fz.out("signalled>0", signalled > 0);
        peer.close();
        client.close();
        server.close();
        Fz.done();
    }
}
