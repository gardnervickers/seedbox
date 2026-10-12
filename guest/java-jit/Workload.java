import java.io.*;
import java.lang.management.ManagementFactory;
import java.net.*;
import java.nio.file.*;
import java.time.Duration;
import jdk.jfr.Recording;

/** Normal-tiered Java work plus real JFR start/stop/dump and safepoints. */
public final class Workload {
    static volatile long completed;
    static long work(long seed) {
        long sum = 0;
        for (int i = 0; i < 20_000; i++) {
            long x = (i + seed) * 0x9e3779b97f4a7c15L;
            sum += Long.rotateLeft(x, 17) ^ (x >>> 13);
        }
        completed = sum;
        return sum;
    }
    public static void main(String[] args) throws Exception {
        try (Recording recording = new Recording(); ServerSocket server = new ServerSocket(12345)) {
            recording.enable("jdk.ExecutionSample").withPeriod(Duration.ofMillis(10));
            recording.enable("jdk.GarbageCollection");
            recording.start();
            System.out.println("java-jit: JFR RUNNING compiler=" +
                ManagementFactory.getCompilationMXBean().getName());
            boolean stop = false;
            while (!stop) {
                try (Socket socket = server.accept();
                     BufferedReader in = new BufferedReader(new InputStreamReader(socket.getInputStream()));
                     PrintWriter out = new PrintWriter(socket.getOutputStream(), true)) {
                    String command = in.readLine();
                    if (command.equals("stop")) {
                        System.gc();
                        recording.stop();
                        Path file = Path.of("/tmp/workload.jfr");
                        recording.dump(file);
                        long bytes = Files.size(file);
                        if (bytes == 0) throw new AssertionError("empty JFR recording");
                        out.println("JFR STOPPED bytes=" + bytes);
                        System.out.println("java-jit: JFR STOPPED bytes=" + bytes);
                        stop = true;
                    } else {
                        long seed = Long.parseLong(command);
                        long sum = 0;
                        for (int round = 0; round < 100; round++) sum += work(seed);
                        out.println(Long.toUnsignedString(sum));
                        System.out.println("java-jit: seed=" + seed + " checksum=" + Long.toUnsignedString(sum));
                    }
                }
            }
        }
        System.out.println("java-jit: completed");
    }
}
