import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Class redefinition under compiled callers: workers run Target.compute hot while the main
 * thread alternates its bytecode between two versions and retransforms it through a no-op
 * transformer. Each redefinition deoptimizes dependent code at a safepoint. Every result must
 * come from one of the two versions; the counts do not depend on timing.
 */
public final class Redefine {
    static final int K1 = 0x5EED0001, K2 = 0x5EED0002;

    static byte[] patch(byte[] v1) {
        byte[] v2 = v1.clone();
        int hits = 0;
        // The body's CONSTANT_Integer 0x5EED0001 becomes 0x5EED0002.
        for (int i = 0; i + 4 < v2.length; i++) {
            if (v2[i] == 3 && v2[i + 1] == 0x5E && v2[i + 2] == (byte) 0xED && v2[i + 3] == 0 && v2[i + 4] == 1) {
                v2[i + 4] = 2;
                hits++;
            }
        }
        Fz.expect(hits == 1, "one patchable constant, found " + hits);
        return v2;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Redefine", args);
        Instrumentation inst = Agent.inst;
        Fz.expect(inst != null && inst.isRedefineClassesSupported() && inst.isRetransformClassesSupported(),
                  "agent with redefinition");
        byte[] v1;
        try (InputStream in = Redefine.class.getResourceAsStream("/Target.class")) { v1 = in.readAllBytes(); }
        byte[] v2 = patch(v1);
        inst.addTransformer(new ClassFileTransformer() {
            public byte[] transform(Module m, ClassLoader l, String name, Class<?> c, ProtectionDomain pd, byte[] b) {
                return null;
            }
        }, true);
        int workers = 3, n = 3_000_000 * Fz.scale;
        long[] good = new long[workers], bad = new long[workers];
        AtomicBoolean done = new AtomicBoolean();
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long g = 0, b = 0;
                for (int i = 0; i < n; i++) {
                    int r = Target.compute(i);
                    if (r == i * 3 + K1 || r == i * 3 + K2) g++;
                    else b++;
                }
                good[id] = g;
                bad[id] = b;
            });
            ts[w].start();
        }
        int k = 0;
        while (ts[0].isAlive() || ts[1].isAlive() || ts[2].isAlive()) {
            inst.redefineClasses(new ClassDefinition(Target.class, (k & 1) == 0 ? v2 : v1));
            if ((k & 3) == 3) inst.retransformClasses(Target.class);
            k++;
            Thread.sleep(5);
        }
        Fz.join(ts);
        Fz.expect(k > 0, "at least one redefinition");
        for (int w = 0; w < workers; w++) Fz.out("worker" + w, good[w] + " good " + bad[w] + " bad");
        Fz.done();
    }
}
