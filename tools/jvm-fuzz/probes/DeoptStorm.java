/**
 * Implicit null checks taken from compiled code over and over. With the deoptstorm profile
 * (unlimited traps and recompilations) every one is a SIGSEGV, an uncommon trap, a
 * not-entrant patch and a recompilation; a second thread keeps running compiled code.
 */
public final class DeoptStorm {
    static final class Box { int v; Box next; Box(int v) { this.v = v; } }
    static volatile boolean stop;

    static int f0(Box b) { return b.v + 1; }
    static int f1(Box b) { return b.next.v * 3; }
    static int f2(int[] a) { return a.length + a[0]; }
    static int f3(Box b) { return b.v ^ 0x55; }
    static int f4(String s) { return s.length(); }
    static int f5(Box b) { return b.next.next.v; }
    static int f6(long[] a) { return (int) a[a.length - 1]; }
    static int f7(Object o) { return o.hashCode() & 0; }

    public static void main(String[] args) throws Exception {
        Fz.init("DeoptStorm", args);
        Box chain = new Box(1);
        chain.next = new Box(2);
        chain.next.next = new Box(3);
        Box shortChain = new Box(4);
        int[] ints = {9};
        long[] longs = {11};
        long[] busy = {0};
        Thread other = new Thread(() -> {
            long h = 1;
            while (!stop) for (int i = 0; i < 10_000; i++) h = h * 31 + (i ^ (h >>> 7));
            busy[0] = h;
        });
        other.start();
        long sum = 0;
        int npes = 0;
        for (int i = 0; i < 300_000 * Fz.scale; i++) {
            boolean nul = i % 61 == 0;
            try {
                switch (i & 7) {
                    case 0 -> sum += f0(nul ? null : chain);
                    case 1 -> sum += f1(nul ? shortChain : chain);
                    case 2 -> sum += f2(nul ? null : ints);
                    case 3 -> sum += f3(nul ? null : chain);
                    case 4 -> sum += f4(nul ? null : "abc");
                    case 5 -> sum += f5(nul ? shortChain : chain);
                    case 6 -> sum += f6(nul ? null : longs);
                    default -> sum += f7(nul ? null : chain);
                }
            } catch (NullPointerException e) {
                // Whether the exception has a stack trace depends on OmitStackTraceInFastThrow.
                npes++;
            }
        }
        stop = true;
        other.join();
        Fz.out("sum", sum);
        Fz.out("npes", npes);
        Fz.done();
    }
}
