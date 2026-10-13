import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicLongArray;
import java.util.function.LongUnaryOperator;
import jdk.test.whitebox.WhiteBox;

/**
 * Generation-tagged code reuse in a small unsegmented code cache. Each generation is a hidden
 * copy of GenPayload whose one constant carries the generation number, so copies compile to
 * near-identical code that differs only in that constant. The main thread compiles each copy at
 * C2 (WhiteBox), publishes it with its generation as one immutable pair, waits until every reader
 * has run it, requires that it is still C2 code, drops it and collects, so later generations are
 * compiled where dropped ones were. Readers check every call against an independent scalar model
 * of that generation: a stale translation of reused code returns another generation's value.
 * -Dfz.diag=true (never set by the corpus: addresses differ from run to run) reports on stderr
 * which generations' compiled entries landed exactly where an earlier generation's had been.
 */
public final class CodeReuseGenerations {
    static final long TAG = 0x5EED6E4E00000000L;
    static final int STEPS = 24;

    /** The scalar model of GenPayload.applyAsLong for generation gen. */
    static long model(long x, long gen) {
        long g = TAG | gen, h = x;
        for (int k = 0; k < STEPS; k++) {
            h ^= h >>> (13 + k % 20);
            h *= 0x9E3779B97F4A7C15L + 2L * k * 0x10001;
            h += g;
            h = Long.rotateLeft(h, 7 + (k * 5) % 50) ^ k;
        }
        return h;
    }

    static byte[] patch(byte[] bytes, int gen) {
        byte[] out = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 8 < out.length; i++) {
            if (out[i] == 5 && out[i + 1] == 0x5E && out[i + 2] == (byte) 0xED && out[i + 3] == 0x6E && out[i + 4] == 0x4E
                && out[i + 5] == 0 && out[i + 6] == 0 && out[i + 7] == 0 && out[i + 8] == 0) {
                out[i + 5] = (byte) (gen >>> 24);
                out[i + 6] = (byte) (gen >>> 16);
                out[i + 7] = (byte) (gen >>> 8);
                out[i + 8] = (byte) gen;
                hits++;
            }
        }
        Fz.expect(hits == 1, "one patchable constant, found " + hits);
        return out;
    }

    record Pub(LongUnaryOperator op, int gen) {}

    static volatile Pub pub;
    static volatile boolean stop;
    static final boolean DIAG = Boolean.getBoolean("fz.diag");

    static void diag(String s) { if (DIAG) System.err.println("diag " + s); }

    /** Compiles m at C2, repeating a request dropped while another compilation of m was queued. */
    static boolean compileC2(WhiteBox wb, Method m) throws InterruptedException {
        for (int w = 0; w < 60_000 && wb.getMethodCompilationLevel(m) != 4; w++) {
            if (w % 10 == 0 && !wb.isMethodQueuedForCompilation(m)) wb.enqueueMethodForCompilation(m, 4);
            Thread.sleep(1);
        }
        return wb.getMethodCompilationLevel(m) == 4;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("CodeReuseGenerations", args);
        WhiteBox wb = WhiteBox.getWhiteBox();
        boolean tiered = wb.getBooleanVMFlag("TieredCompilation");
        boolean c2 = wb.getBooleanVMFlag("UseCompiler") && (!tiered || wb.getIntxVMFlag("TieredStopAtLevel") >= 4);
        byte[] template;
        try (InputStream in = CodeReuseGenerations.class.getResourceAsStream("/GenPayload.class")) { template = in.readAllBytes(); }
        int readers = 2;
        AtomicLongArray seen = new AtomicLongArray(readers);
        long[] bad = new long[readers];
        Thread[] ts = new Thread[readers];
        for (int r = 0; r < readers; r++) {
            final int id = r;
            ts[r] = new Thread(() -> {
                long x = Fz.seed * 7919 + id;
                while (!stop) {
                    Pub p = pub;
                    if (p == null) { Thread.onSpinWait(); continue; }
                    for (int j = 0; j < 128; j++, x++) {
                        if (p.op().applyAsLong(x) != model(x, p.gen())) bad[id]++;
                    }
                    seen.set(id, p.gen());
                }
            }, "fz-reader-" + r);
            ts[r].start();
        }
        int gens = 60 * Fz.scale, keepLive = 2;
        ArrayDeque<Pub> live = new ArrayDeque<>();
        List<WeakReference<Class<?>>> dropped = new ArrayList<>();
        Map<Long, Integer> entries = new HashMap<>();  // diagnostics: entry address -> generation
        long direct = 0, notC2 = 0, reused = 0;
        for (int g = 1; g <= gens; g++) {
            Class<?> c = MethodHandles.lookup().defineHiddenClass(patch(template, g), false).lookupClass();
            LongUnaryOperator op = (LongUnaryOperator) c.getDeclaredConstructor().newInstance();
            Method m = c.getDeclaredMethod("applyAsLong", long.class);
            for (long x = 0; x < 64; x++) if (op.applyAsLong(x) != model(x, g)) direct++;
            if (c2) {
                Fz.expect(compileC2(wb, m), "generation " + g + " reached C2");
                if (DIAG) {
                    Object[] nm = wb.getNMethod(m, false);
                    if (nm != null && nm.length > 4 && nm[4] instanceof Long entry) {
                        Integer prev = entries.put(entry, g);
                        if (prev != null) reused++;
                        diag("gen " + g + " entry " + Long.toHexString(entry) + (prev != null ? " reuses gen " + prev : ""));
                    }
                }
            } else {
                for (long x = 0; x < 20_000; x++) op.applyAsLong(x);
            }
            for (long x = 64; x < 128; x++) if (op.applyAsLong(x) != model(x, g)) direct++;
            pub = new Pub(op, g);
            live.addLast(pub);
            for (int r = 0; r < readers; r++) while (seen.get(r) != g) Thread.sleep(1);
            if (c2 && wb.getMethodCompilationLevel(m) != 4) notC2++;
            while (live.size() > keepLive) {
                Pub old = live.removeFirst();
                dropped.add(new WeakReference<>(old.op().getClass()));
            }
            if (g % 4 == 0) {
                wb.fullGC();
                if (DIAG) {
                    long blob = wb.allocateCodeBlob(4096, 0);
                    Integer prev = entries.get(blob);
                    diag("blob " + Long.toHexString(blob) + (prev != null ? " at gen " + prev + "'s entry" : ""));
                    if (blob != 0) wb.freeCodeBlob(blob);
                    for (int type = 0; type < 4; type++) {
                        Object[] heap = wb.getCodeHeapEntries(type);
                        diag("heap " + type + " entries " + (heap == null ? -1 : heap.length));
                    }
                }
            }
        }
        stop = true;
        Fz.join(ts);
        pub = null;
        live.clear();
        for (int i = 0; i < 3; i++) wb.fullGC();
        long unloaded = dropped.stream().filter(w -> w.get() == null).count();
        diag("reused entries " + reused + " of " + gens + ", unloaded " + unloaded + " of " + dropped.size());
        Fz.out("generations", gens);
        Fz.out("directMismatches", direct);
        Fz.out("readerMismatches", bad[0] + bad[1]);
        Fz.out("lostC2", notC2);
        Fz.out("mostUnloaded", unloaded >= dropped.size() - 2);
        Fz.mix(gens);
        Fz.mix(direct + bad[0] + bad[1] + notC2);
        Fz.done();
    }
}

/** Copied by CodeReuseGenerations with GEN's low word set to the generation number. */
final class GenPayload implements LongUnaryOperator {
    static final long GEN = 0x5EED6E4E00000000L;

    public long applyAsLong(long x) {
        long h = x;
        h ^= h >>> 13; h *= 0x9e3779b97f4a7c15L; h += GEN; h = Long.rotateLeft(h, 7) ^ 0;
        h ^= h >>> 14; h *= 0x9e3779b97f4c7c17L; h += GEN; h = Long.rotateLeft(h, 12) ^ 1;
        h ^= h >>> 15; h *= 0x9e3779b97f4e7c19L; h += GEN; h = Long.rotateLeft(h, 17) ^ 2;
        h ^= h >>> 16; h *= 0x9e3779b97f507c1bL; h += GEN; h = Long.rotateLeft(h, 22) ^ 3;
        h ^= h >>> 17; h *= 0x9e3779b97f527c1dL; h += GEN; h = Long.rotateLeft(h, 27) ^ 4;
        h ^= h >>> 18; h *= 0x9e3779b97f547c1fL; h += GEN; h = Long.rotateLeft(h, 32) ^ 5;
        h ^= h >>> 19; h *= 0x9e3779b97f567c21L; h += GEN; h = Long.rotateLeft(h, 37) ^ 6;
        h ^= h >>> 20; h *= 0x9e3779b97f587c23L; h += GEN; h = Long.rotateLeft(h, 42) ^ 7;
        h ^= h >>> 21; h *= 0x9e3779b97f5a7c25L; h += GEN; h = Long.rotateLeft(h, 47) ^ 8;
        h ^= h >>> 22; h *= 0x9e3779b97f5c7c27L; h += GEN; h = Long.rotateLeft(h, 52) ^ 9;
        h ^= h >>> 23; h *= 0x9e3779b97f5e7c29L; h += GEN; h = Long.rotateLeft(h, 7) ^ 10;
        h ^= h >>> 24; h *= 0x9e3779b97f607c2bL; h += GEN; h = Long.rotateLeft(h, 12) ^ 11;
        h ^= h >>> 25; h *= 0x9e3779b97f627c2dL; h += GEN; h = Long.rotateLeft(h, 17) ^ 12;
        h ^= h >>> 26; h *= 0x9e3779b97f647c2fL; h += GEN; h = Long.rotateLeft(h, 22) ^ 13;
        h ^= h >>> 27; h *= 0x9e3779b97f667c31L; h += GEN; h = Long.rotateLeft(h, 27) ^ 14;
        h ^= h >>> 28; h *= 0x9e3779b97f687c33L; h += GEN; h = Long.rotateLeft(h, 32) ^ 15;
        h ^= h >>> 29; h *= 0x9e3779b97f6a7c35L; h += GEN; h = Long.rotateLeft(h, 37) ^ 16;
        h ^= h >>> 30; h *= 0x9e3779b97f6c7c37L; h += GEN; h = Long.rotateLeft(h, 42) ^ 17;
        h ^= h >>> 31; h *= 0x9e3779b97f6e7c39L; h += GEN; h = Long.rotateLeft(h, 47) ^ 18;
        h ^= h >>> 32; h *= 0x9e3779b97f707c3bL; h += GEN; h = Long.rotateLeft(h, 52) ^ 19;
        h ^= h >>> 13; h *= 0x9e3779b97f727c3dL; h += GEN; h = Long.rotateLeft(h, 7) ^ 20;
        h ^= h >>> 14; h *= 0x9e3779b97f747c3fL; h += GEN; h = Long.rotateLeft(h, 12) ^ 21;
        h ^= h >>> 15; h *= 0x9e3779b97f767c41L; h += GEN; h = Long.rotateLeft(h, 17) ^ 22;
        h ^= h >>> 16; h *= 0x9e3779b97f787c43L; h += GEN; h = Long.rotateLeft(h, 22) ^ 23;
        return h;
    }
}
