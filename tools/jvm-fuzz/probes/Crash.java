import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.List;
import sun.misc.Unsafe;

/**
 * Deliberate JVM deaths whose report is the evidence: a store to address 0 from interpreted or
 * C2-compiled code (SIGSEGV through HotSpot's handler and error reporter), heap exhaustion
 * under -XX:+CrashOnOutOfMemoryError, and Runtime.halt from a shutdown hook. The exit status is
 * part of the expected result; the report is only compared with the replay.
 * Third argument: segv-int, segv-c2, oom or halthook.
 */
public final class Crash {
    static Unsafe u;

    static void store(long address, long value) { u.putLong(address, value); }

    public static void main(String[] args) throws Exception {
        Fz.init("Crash", args);
        Field f = Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        u = (Unsafe) f.get(null);
        String mode = args.length > 2 ? args[2] : "segv-int";
        System.out.println("mode " + mode);
        System.out.flush();
        switch (mode) {
            case "segv-int" -> store(0, 1);
            case "segv-c2" -> {
                long scratch = u.allocateMemory(8);
                for (int i = 0; i < 200_000; i++) store(scratch, i);
                store(0, 1);
            }
            case "oom" -> {
                List<long[]> hog = new ArrayList<>();
                while (true) hog.add(new long[1 << 16]);
            }
            default -> {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> Runtime.getRuntime().halt(3)));
                System.exit(0);
            }
        }
    }
}
