import java.lang.management.ManagementFactory;
import java.rmi.Remote;
import java.rmi.RemoteException;
import java.rmi.registry.LocateRegistry;
import java.rmi.registry.Registry;
import java.rmi.server.UnicastRemoteObject;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.AttributeChangeNotification;
import javax.management.MBeanServer;
import javax.management.MBeanServerConnection;
import javax.management.Notification;
import javax.management.NotificationBroadcasterSupport;
import javax.management.NotificationListener;
import javax.management.ObjectName;
import javax.management.remote.JMXConnector;
import javax.management.remote.JMXConnectorFactory;
import javax.management.remote.JMXConnectorServer;
import javax.management.remote.JMXConnectorServerFactory;
import javax.management.remote.JMXServiceURL;

/**
 * Local RMI and JMX over loopback: an anonymous-port registry, a remote object with serialized
 * arguments, results and remote exceptions, callbacks from server to an exported client object,
 * distributed GC as stubs are dropped, then a JMX RMI connector server with a custom MBean whose
 * attributes, operations and notifications are used through a remote connection, and orderly
 * shutdown so no RMI thread keeps the JVM alive. Sets java.rmi.server.hostname=127.0.0.1 itself.
 */
public final class JmxRmi {
    public interface Calc extends Remote {
        long[] mix(long[] in, String tag) throws RemoteException;
        int callback(Sink s, int n) throws RemoteException;
        void fail(String why) throws RemoteException;
    }

    public interface Sink extends Remote {
        void accept(int v) throws RemoteException;
    }

    public static final class CalcImpl implements Calc {
        @Override public long[] mix(long[] in, String tag) {
            long[] out = new long[in.length];
            for (int i = 0; i < in.length; i++) out[i] = in[i] * 31 + tag.hashCode() + i;
            return out;
        }
        @Override public int callback(Sink s, int n) throws RemoteException {
            int sum = 0;
            for (int i = 0; i < n; i++) { s.accept(i); sum += i; }
            return sum;
        }
        @Override public void fail(String why) { throw new IllegalStateException(why); }
    }

    public static final class SinkImpl implements Sink {
        final AtomicInteger total = new AtomicInteger();
        @Override public void accept(int v) { total.addAndGet(v); }
    }

    public interface CounterMBean {
        long getValue();
        void setValue(long v);
        String getName();
        long add(long d);
    }

    public static final class Counter extends NotificationBroadcasterSupport implements CounterMBean {
        long value;
        long seq;
        @Override public synchronized long getValue() { return value; }
        @Override public synchronized void setValue(long v) {
            long old = value;
            value = v;
            sendNotification(new AttributeChangeNotification(this, ++seq, seq, "set", "Value", "long", old, v));
        }
        @Override public String getName() { return "fz-counter"; }
        @Override public synchronized long add(long d) { setValue(value + d); return value; }
    }

    /** Binds the registry to loopback and records the port the kernel picked. */
    static final class LoopbackFactory implements java.rmi.server.RMIServerSocketFactory {
        volatile int port;
        @Override public java.net.ServerSocket createServerSocket(int p) throws java.io.IOException {
            java.net.ServerSocket ss = new java.net.ServerSocket(p, 50, java.net.InetAddress.getLoopbackAddress());
            port = ss.getLocalPort();
            return ss;
        }
    }

    static void rmi() throws Exception {
        LoopbackFactory ssf = new LoopbackFactory();
        Registry reg = LocateRegistry.createRegistry(0, null, ssf);
        CalcImpl impl = new CalcImpl();
        Calc stub = (Calc) UnicastRemoteObject.exportObject(impl, 0);
        reg.rebind("calc", stub);
        Registry client = LocateRegistry.getRegistry("127.0.0.1", ssf.port);
        Calc remote = (Calc) client.lookup("calc");
        Fz.out("rmi.names", String.join(",", client.list()));
        long h = 0;
        for (int i = 0; i < 30 * Fz.scale; i++) {
            long[] in = new long[Fz.rnd.nextInt(2000)];
            for (int k = 0; k < in.length; k++) in[k] = Fz.rnd.nextLong();
            String tag = "t" + Fz.rnd.nextInt(100);
            long[] out = remote.mix(in, tag);
            Fz.expect(java.util.Arrays.equals(out, impl.mix(in, tag)), "remote result");
            h = h * 31 + java.util.Arrays.hashCode(out);
        }
        Fz.out("rmi.results", Long.toHexString(h));
        Fz.mix(h);
        SinkImpl sink = new SinkImpl();
        Sink sinkStub = (Sink) UnicastRemoteObject.exportObject(sink, 0);
        int n = 50 + Fz.rnd.nextInt(50);
        int sum = remote.callback(sinkStub, n);
        Fz.out("rmi.callback", sum + " " + (sink.total.get() == sum));
        String failure;
        try {
            remote.fail("boom" + n);
            failure = "none";
        } catch (IllegalStateException e) {
            failure = e.getClass().getSimpleName() + ":" + e.getMessage();
        }
        Fz.out("rmi.remoteException", failure);
        try {
            client.lookup("missing");
        } catch (java.rmi.NotBoundException e) {
            Fz.out("rmi.notBound", e.getMessage());
        }
        // Drop the client stubs; DGC clean calls flow back to the exporting side.
        remote = null;
        System.gc();
        Fz.expect(UnicastRemoteObject.unexportObject(sink, true), "unexport sink");
        reg.unbind("calc");
        Fz.expect(UnicastRemoteObject.unexportObject(impl, true), "unexport calc");
        Fz.expect(UnicastRemoteObject.unexportObject(reg, true), "unexport registry");
        // A reused connection reports NoSuchObjectException, a new one ConnectException.
        boolean refused;
        try {
            client.list();
            refused = false;
        } catch (RemoteException e) {
            refused = true;
        }
        Fz.out("rmi.refusedAfterUnexport", refused);
    }

    static void jmx() throws Exception {
        MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        ObjectName name = new ObjectName("fz:type=Counter");
        Counter counter = new Counter();
        mbs.registerMBean(counter, name);
        JMXConnectorServer server = JMXConnectorServerFactory.newJMXConnectorServer(
            new JMXServiceURL("service:jmx:rmi://127.0.0.1"), null, mbs);
        server.start();
        int updates = 10 + Fz.rnd.nextInt(20);
        long expect = 0;
        try (JMXConnector c = JMXConnectorFactory.connect(server.getAddress())) {
            MBeanServerConnection conn = c.getMBeanServerConnection();
            Object vm = conn.getAttribute(new ObjectName("java.lang:type=Runtime"), "VmName");
            Fz.out("jmx.vmName", vm.toString().contains("VM"));
            Fz.out("jmx.counterName", conn.getAttribute(name, "Name"));
            List<Long> seen = new ArrayList<>();
            CountDownLatch all = new CountDownLatch(updates);
            NotificationListener l = (Notification nn, Object hb) -> {
                if (nn instanceof AttributeChangeNotification a) {
                    synchronized (seen) {
                        seen.add((Long) a.getNewValue());
                    }
                    all.countDown();
                }
            };
            conn.addNotificationListener(name, l, null, null);
            for (int i = 0; i < updates; i++) {
                long d = Fz.rnd.nextInt(1000);
                if (i % 2 == 0) {
                    expect += d;
                    conn.invoke(name, "add", new Object[] {d}, new String[] {"long"});
                } else {
                    expect = d * 3;
                    conn.setAttribute(name, new javax.management.Attribute("Value", d * 3));
                }
            }
            boolean got = all.await(60, TimeUnit.SECONDS);
            Fz.out("jmx.value", conn.getAttribute(name, "Value") + " " + expect);
            synchronized (seen) {
                Fz.out("jmx.notifications", got + " " + seen.size() + " last=" + seen.get(seen.size() - 1));
                long h = 0;
                for (long v : seen) h = h * 31 + v;
                Fz.mix(h);
            }
            conn.removeNotificationListener(name, l);
            Fz.out("jmx.mbeanCountPositive", conn.getMBeanCount() > 10);
        } finally {
            server.stop();
        }
        mbs.unregisterMBean(name);
        Fz.out("jmx.stopped", !server.isActive());
        Fz.expect(counter.getValue() == expect, "counter value");
        Fz.mix(expect);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("JmxRmi", args);
        System.setProperty("java.rmi.server.hostname", "127.0.0.1");
        rmi();
        jmx();
        Fz.done();
    }
}
