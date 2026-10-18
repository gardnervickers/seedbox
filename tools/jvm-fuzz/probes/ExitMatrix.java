/**
 * JVM exits while other threads run compiled loops: main returns with daemons spinning,
 * System.exit from a non-main thread, Runtime.halt, and an uncaught exception in main. The exit
 * code is part of the expected output (the probe is marked any_exit).
 * Third argument: daemons, exit7, halt5 or uncaught.
 */
public final class ExitMatrix {
    static volatile long sink;

    static void spinners(int n, boolean daemon) {
        for (int i = 0; i < n; i++) {
            Thread t = new Thread(() -> { long h = 1; while (true) { h = h * 31 + 7; if (h == 0) sink = h; } });
            t.setDaemon(daemon);
            t.start();
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("ExitMatrix", args);
        String mode = args.length > 2 ? args[2] : "daemons";
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook " + mode)));
        System.out.println("mode " + mode);
        switch (mode) {
            case "daemons" -> { spinners(3, true); Thread.sleep(50); }
            case "exit7" -> {
                spinners(3, true);
                Thread t = new Thread(() -> System.exit(7));
                t.start();
                t.join();
            }
            case "halt5" -> { spinners(3, true); Thread.sleep(20); Runtime.getRuntime().halt(5); }
            default -> { spinners(2, true); throw new IllegalStateException("uncaught in main"); }
        }
    }
}
