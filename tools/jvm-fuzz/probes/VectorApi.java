import jdk.incubator.vector.FloatVector;
import jdk.incubator.vector.IntVector;
import jdk.incubator.vector.LongVector;
import jdk.incubator.vector.VectorMask;
import jdk.incubator.vector.VectorOperators;
import jdk.incubator.vector.VectorSpecies;

/** Explicit SIMD through the incubating Vector API (requires --add-modules jdk.incubator.vector). */
public final class VectorApi {
    static final VectorSpecies<Float> F = FloatVector.SPECIES_PREFERRED;
    static final VectorSpecies<Integer> I = IntVector.SPECIES_PREFERRED;
    static final VectorSpecies<Long> L = LongVector.SPECIES_PREFERRED;

    static float kernel(float[] a, float[] b, float[] c) {
        FloatVector acc = FloatVector.zero(F);
        int i = 0;
        for (; i < F.loopBound(a.length); i += F.length()) {
            FloatVector va = FloatVector.fromArray(F, a, i), vb = FloatVector.fromArray(F, b, i);
            FloatVector r = va.fma(vb, va.neg()).max(vb).sqrt();
            r.intoArray(c, i);
            acc = acc.add(r);
        }
        float s = acc.reduceLanes(VectorOperators.ADD);
        for (; i < a.length; i++) s += c[i] = (float) Math.sqrt(Math.max(a[i] * b[i] - a[i], b[i]));
        return s;
    }

    static long ints(int[] a, int[] b) {
        long s = 0;
        int i = 0;
        for (; i < I.loopBound(a.length); i += I.length()) {
            IntVector va = IntVector.fromArray(I, a, i), vb = IntVector.fromArray(I, b, i);
            VectorMask<Integer> m = va.compare(VectorOperators.GT, vb);
            IntVector r = va.lanewise(VectorOperators.XOR, vb).blend(va.mul(vb), m).lanewise(VectorOperators.ROL, 3);
            s += r.reduceLanes(VectorOperators.ADD) + m.trueCount();
        }
        return s;
    }

    static long longs(long[] a) {
        LongVector acc = LongVector.zero(L);
        for (int i = 0; i < L.loopBound(a.length); i += L.length()) acc = acc.add(LongVector.fromArray(L, a, i).lanewise(VectorOperators.ASHR, 3));
        return acc.reduceLanes(VectorOperators.XOR);
    }

    public static void main(String[] args) {
        Fz.init("VectorApi", args);
        int n = 4096 + Fz.rnd.nextInt(64);
        float[] a = new float[n], b = new float[n], c = new float[n];
        int[] ia = new int[n], ib = new int[n];
        long[] la = new long[n];
        for (int i = 0; i < n; i++) {
            a[i] = (float) Fz.rnd.nextGaussian(); b[i] = Fz.rnd.nextFloat();
            ia[i] = Fz.edgeInt(); ib[i] = Fz.rnd.nextInt(); la[i] = Fz.edgeLong();
        }
        long first = 0;
        for (int r = 0; r < 2_000 * Fz.scale; r++) {
            long h = Float.floatToIntBits(kernel(a, b, c)) * 31L + ints(ia, ib) * 7 + longs(la);
            if (r == 0) first = h;
            else Fz.expect(h == first, "round " + r + " differs");
        }
        Fz.outHex("vectors", first);
        Fz.out("species", F.length() + " " + I.length() + " " + L.length());
        Fz.done();
    }
}
