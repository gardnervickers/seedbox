import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.stream.Stream;

/** Thread-directed signals for preview probes: tgkill through the foreign-function API. */
final class Native {
    static final int SIGHUP = 1, SIGWINCH = 28;
    private static final MethodHandle TGKILL;
    static {
        Linker linker = Linker.nativeLinker();
        TGKILL = linker.downcallHandle(linker.defaultLookup().find("tgkill").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
    }

    /** Sends `sig` to Linux thread `tid` of this process; false if the thread is gone. */
    static boolean tgkill(int tid, int sig) {
        try {
            return (int) TGKILL.invokeExact((int) ProcessHandle.current().pid(), tid, sig) == 0;
        } catch (Throwable t) {
            throw new RuntimeException(t);
        }
    }

    /** The Linux thread id of the started Java thread named `name` (at most 15 characters). */
    static int tid(String name) throws Exception {
        for (int attempt = 0; attempt < 2000; attempt++) {
            try (Stream<Path> tasks = Files.list(Path.of("/proc/self/task"))) {
                for (Path task : (Iterable<Path>) tasks::iterator) {
                    try {
                        if (Files.readString(task.resolve("comm")).trim().equals(name))
                            return Integer.parseInt(task.getFileName().toString());
                    } catch (java.io.IOException e) {
                        // The thread exited while we listed it.
                    }
                }
            }
            Thread.sleep(1);
        }
        throw new IllegalStateException("no thread named " + name);
    }
}
