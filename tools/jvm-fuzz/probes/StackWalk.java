import java.util.stream.Collectors;

/** Stack traces and StackWalker frames from interpreted, compiled and inlined code. */
public final class StackWalk {
    static String a(int i) { return b(i + 1); }
    static String b(int i) { return c(i * 2); }
    static String c(int i) {
        if (i % 1000 == 8) {
            return StackWalker.getInstance().walk(s -> s.limit(4).map(f -> f.getMethodName() + ":" + f.getLineNumber()).collect(Collectors.joining(",")));
        }
        if (i % 1000 == 10) {
            StackTraceElement[] st = new Throwable().getStackTrace();
            StringBuilder sb = new StringBuilder();
            for (int k = 0; k < 4; k++) sb.append(st[k].getMethodName()).append(':').append(st[k].getLineNumber()).append(',');
            return sb.toString();
        }
        return null;
    }

    public static void main(String[] args) {
        Fz.init("StackWalk", args);
        java.util.TreeSet<String> seen = new java.util.TreeSet<>();
        for (int i = 0; i < 400_000 * Fz.scale; i++) {
            String s = a(i);
            if (s != null) seen.add(s);
        }
        for (String s : seen) Fz.out("frames", s);
        Fz.done();
    }
}
