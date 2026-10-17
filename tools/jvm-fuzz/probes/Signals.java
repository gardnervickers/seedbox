import java.util.concurrent.Semaphore;
import sun.misc.Signal;

/** Java-level signal handlers raised from the process itself, and shutdown hooks. */
public final class Signals {
    public static void main(String[] args) throws Exception {
        Fz.init("Signals", args);
        Semaphore handled = new Semaphore(0);
        int[] counts = new int[2];
        Signal.handle(new Signal("HUP"), s -> { counts[0]++; handled.release(); });
        Signal.handle(new Signal("WINCH"), s -> { counts[1]++; handled.release(); });
        int n = 200 * Fz.scale;
        for (int i = 0; i < n; i++) {
            Signal.raise(new Signal((i & 1) == 0 ? "HUP" : "WINCH"));
            handled.acquire();
        }
        Fz.out("handled", counts[0] + " " + counts[1]);
        Process self = new ProcessBuilder("/bin/sh", "-c", "kill -HUP " + ProcessHandle.current().pid()).start();
        self.waitFor();
        handled.acquire();
        Fz.out("external", counts[0]);
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook ran")));
        Fz.done();
    }
}
