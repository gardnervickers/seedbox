import java.util.ArrayList;
import java.util.List;

/** Throw/catch in hot code, nested finally, suppressed exceptions and stack-overflow recovery. */
public final class Exceptions {
    static final class Res implements AutoCloseable {
        final int id;
        final List<String> log;
        Res(int id, List<String> log) { this.id = id; this.log = log; }
        public void close() {
            log.add("close" + id);
            if (id % 3 == 0) throw new IllegalStateException("close" + id);
        }
    }

    static int depth;

    static void recurse(long a, long b, long c) {
        depth++;
        recurse(a + 1, b ^ a, c * 3);
    }

    static int thrower(int i) {
        switch (i % 6) {
            case 0: return 10 / (i % 2);
            case 1: return new int[i % 7 - 3][1].length + new int[2][i % 3 - 1].length;
            case 2: return ((String) (Object) Integer.valueOf(i)).length();
            case 3: throw new UnsupportedOperationException("u" + (i % 5));
            case 4: return Integer.parseInt("x" + i);
            default: return ((Object[]) new String[1])[0].hashCode();
        }
    }

    static long finallyChain(int i) {
        long r = i;
        try {
            try {
                if (i % 2 == 0) throw new RuntimeException("a");
                r += 3;
            } finally {
                r *= 7;
                if (i % 5 == 0) throw new IllegalArgumentException("b");
            }
        } catch (IllegalArgumentException e) {
            r += 11;
        } catch (RuntimeException e) {
            r -= 13;
        } finally {
            r ^= 0x55;
        }
        return r;
    }

    public static void main(String[] args) {
        Fz.init("Exceptions", args);
        int[] kinds = new int[8];
        for (int i = 0; i < 60_000 * Fz.scale; i++) {
            try {
                Fz.mix(thrower(i));
            } catch (ArithmeticException e) {
                kinds[0]++;
            } catch (NegativeArraySizeException | ArrayIndexOutOfBoundsException e) {
                kinds[1]++;
            } catch (ClassCastException e) {
                kinds[2]++;
            } catch (UnsupportedOperationException e) {
                kinds[3]++;
                Fz.mix(e.getMessage());
            } catch (NumberFormatException e) {
                kinds[4]++;
            } catch (NullPointerException e) {
                kinds[5]++;
            }
        }
        Fz.out("kinds", java.util.Arrays.toString(kinds));
        long f = 0;
        for (int i = 0; i < 100_000; i++) f += finallyChain(i);
        Fz.out("finally", f);
        List<String> log = new ArrayList<>();
        int suppressed = 0;
        for (int i = 1; i < 40; i++) {
            try (Res a = new Res(i, log); Res b = new Res(i + 1, log); Res c = new Res(i + 2, log)) {
                if (i % 2 == 0) throw new RuntimeException("body" + i);
            } catch (RuntimeException e) {
                suppressed += e.getSuppressed().length;
                Fz.mix(e.getMessage());
            }
        }
        Fz.out("suppressed", suppressed + " " + log.size());
        int traces = 0;
        for (int i = 0; i < 2_000; i++) {
            Throwable t = new Throwable();
            traces += t.getStackTrace().length > 0 ? 1 : 0;
        }
        Fz.out("traces", traces);
        int overflows = 0;
        for (int i = 0; i < 4 + 2 * Fz.scale; i++) {
            depth = 0;
            try {
                recurse(i, 1, 2);
            } catch (StackOverflowError e) {
                overflows++;
            }
            Fz.expect(depth > 1000, "shallow overflow " + depth);
        }
        Fz.out("overflows", overflows);
        Fz.done();
    }
}
