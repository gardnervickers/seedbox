import java.util.Arrays;
import jdk.incubator.vector.DoubleVector;
import jdk.incubator.vector.IntVector;
import jdk.incubator.vector.LongVector;
import jdk.incubator.vector.VectorMask;
import jdk.incubator.vector.VectorOperators;
import jdk.incubator.vector.VectorShuffle;
import jdk.incubator.vector.VectorSpecies;

/**
 * Mask-dependent Vector API lane movement (requires --add-modules jdk.incubator.vector): masked
 * gathers and scatters (inactive lanes must read as zero and must not be stored), masked loads
 * and stores that straddle the array end, compress/expand, mask compress, rearrange with
 * partial shuffles and selectFrom, over explicit 64-, 128- and 256-bit species. Results are
 * compared lane by lane with scalar models. JDK 21 range-checks every lane of a gather's index
 * map, including inactive ones, so an out-of-range inactive index throws; the probe records
 * that outcome rather than assuming either behavior, and active-lane errors must be reported.
 */
public final class VectorMasks {
    static final VectorSpecies<Integer> I64 = IntVector.SPECIES_64, I128 = IntVector.SPECIES_128, I256 = IntVector.SPECIES_256;
    static final VectorSpecies<Long> L128 = LongVector.SPECIES_128, L256 = LongVector.SPECIES_256;
    static final VectorSpecies<Double> D128 = DoubleVector.SPECIES_128;
    static final int[] BAD = {Integer.MIN_VALUE, Integer.MAX_VALUE, -1, 1 << 20, -65536};

    /** Gather and scatter through masked index maps whose inactive lanes are out of range. */
    static long gathers(VectorSpecies<Integer> s, int iters) {
        int n = s.length();
        int[] src = new int[37];
        for (int i = 0; i < src.length; i++) src[i] = Fz.rnd.nextInt();
        long h = 0;
        int[] idx = new int[n], out = new int[src.length + 1];
        boolean[] mb = new boolean[n];
        for (int k = 0; k < iters; k++) {
            for (int l = 0; l < n; l++) {
                mb[l] = ((k >>> l) & 1) != 0 || l == k % n;
                idx[l] = mb[l] ? (k * 7 + l * 5) % src.length : (k * 11 + l * 13) % src.length;
            }
            VectorMask<Integer> m = VectorMask.fromArray(s, mb, 0);
            IntVector v = IntVector.fromArray(s, src, 0, idx, 0, m);
            for (int l = 0; l < n; l++) {
                int want = mb[l] ? src[idx[l]] : 0;
                if (v.lane(l) != want) Fz.expect(false, "gather lane " + l);
            }
            // Scatter: only active lanes may store; inactive lanes point at a sentinel slot.
            for (int l = 0; l < n; l++) if (!mb[l]) idx[l] = src.length;
            v.add(k).intoArray(out, 0, idx, 0, m);
            if (out[src.length] != 0) Fz.expect(false, "inactive scatter lane stored");
            h = h * 31 + v.reduceLanes(VectorOperators.ADD);
        }
        for (int x : out) h = h * 17 + x;
        return h;
    }

    static long longGathers(VectorSpecies<Long> s, int iters) {
        int n = s.length();
        long[] src = new long[29];
        for (int i = 0; i < src.length; i++) src[i] = Fz.rnd.nextLong();
        int[] idx = new int[n];
        boolean[] mb = new boolean[n];
        long h = 0;
        for (int k = 0; k < iters; k++) {
            for (int l = 0; l < n; l++) {
                mb[l] = ((k + l) % 3) != 0;
                idx[l] = mb[l] ? (k + l * 11) % src.length : (k * 5 + l) % src.length;
            }
            LongVector v = LongVector.fromArray(s, src, 0, idx, 0, VectorMask.fromArray(s, mb, 0));
            for (int l = 0; l < n; l++) if (v.lane(l) != (mb[l] ? src[idx[l]] : 0)) Fz.expect(false, "long gather");
            h = h * 31 + v.reduceLanes(VectorOperators.XOR);
        }
        return h;
    }

    /** Masked loads and stores whose inactive lanes lie beyond the array end. */
    static long tails(VectorSpecies<Integer> s, int iters) {
        int n = s.length();
        long h = 0;
        for (int k = 0; k < iters; k++) {
            int len = 1 + k % (3 * n);
            int[] a = new int[len], b = new int[len];
            for (int i = 0; i < len; i++) a[i] = i * 3 + k;
            for (int off = 0; off < len; off += n) {
                VectorMask<Integer> m = s.indexInRange(off, len);
                IntVector v = IntVector.fromArray(s, a, off, m);
                v.mul(5).intoArray(b, off, m);
            }
            for (int i = 0; i < len; i++) if (b[i] != a[i] * 5) Fz.expect(false, "masked tail");
            h = h * 31 + b[len - 1];
        }
        return h;
    }

    /** compress, expand, mask compress and rearrange, against scalar models. */
    static long lanes(VectorSpecies<Integer> s, int iters) {
        int n = s.length();
        int[] a = new int[n];
        long h = 0;
        for (int k = 0; k < iters; k++) {
            for (int l = 0; l < n; l++) a[l] = k * 131 + l;
            boolean[] mb = new boolean[n];
            for (int l = 0; l < n; l++) mb[l] = ((k * 0x9E3779B9 >>> (l + 3)) & 1) != 0;
            VectorMask<Integer> m = VectorMask.fromArray(s, mb, 0);
            IntVector v = IntVector.fromArray(s, a, 0);
            IntVector c = v.compress(m), e = c.expand(m), round = v.compress(m).expand(m);
            int at = 0;
            for (int l = 0; l < n; l++) {
                int wantC = 0;
                if (l < m.trueCount()) {
                    int seen = -1;
                    for (int q = 0; q < n; q++) if (mb[q] && ++seen == l) wantC = a[q];
                }
                if (c.lane(l) != wantC) Fz.expect(false, "compress lane");
                if (e.lane(l) != (mb[l] ? a[l] : 0) || round.lane(l) != e.lane(l)) Fz.expect(false, "expand lane");
                if (mb[l]) at++;
            }
            VectorMask<Integer> mc = m.compress();
            if (mc.trueCount() != at || (at > 0 && mc.lastTrue() != at - 1)) Fz.expect(false, "mask compress");
            int[] order = new int[n];
            for (int l = 0; l < n; l++) order[l] = (l * 3 + k) % n;
            VectorShuffle<Integer> sh = VectorShuffle.fromArray(s, order, 0);
            IntVector r = v.rearrange(sh, m);
            IntVector sel = IntVector.fromArray(s, order, 0).selectFrom(v);
            for (int l = 0; l < n; l++) {
                if (r.lane(l) != (mb[l] ? a[order[l]] : 0)) Fz.expect(false, "masked rearrange");
                if (sel.lane(l) != a[order[l]]) Fz.expect(false, "selectFrom");
            }
            h = h * 31 + c.reduceLanes(VectorOperators.ADD) + 7L * e.reduceLanes(VectorOperators.XOR) + r.lane(n - 1);
        }
        return h;
    }

    static long doubles(int iters) {
        int n = D128.length();
        double[] a = new double[n];
        long h = 0;
        for (int k = 0; k < iters; k++) {
            for (int l = 0; l < n; l++) a[l] = k + l / 4.0;
            VectorMask<Double> m = VectorMask.fromLong(D128, k & ((1 << n) - 1));
            DoubleVector v = DoubleVector.fromArray(D128, a, 0);
            DoubleVector c = v.compress(m).expand(m);
            for (int l = 0; l < n; l++) if (c.lane(l) != (m.laneIsSet(l) ? a[l] : 0.0)) Fz.expect(false, "double lanes");
            h = h * 31 + Double.doubleToLongBits(c.reduceLanes(VectorOperators.ADD));
        }
        return h;
    }

    /** An active lane with a bad index must still throw, for gathers and masked loads. */
    static String errors() {
        StringBuilder b = new StringBuilder();
        int[] src = {10, 20, 30, 40};
        int[][] cases = {{3, Integer.MAX_VALUE, 1, 0}, {3, 1, 4, 0}, {-1, 0, 0, 0}};
        boolean[] mask = {true, true, true, false};
        for (int[] idx : cases) {
            try {
                IntVector.fromArray(I128, src, 0, idx, 0, VectorMask.fromArray(I128, mask, 0));
                b.append("none ");
            } catch (IndexOutOfBoundsException e) {
                b.append("ioobe ");
            }
        }
        try {
            IntVector.fromArray(I128, src, 2, VectorMask.fromLong(I128, 0b0111));
            b.append("none");
        } catch (IndexOutOfBoundsException e) {
            b.append("ioobe");
        }
        return b.toString();
    }

    public static void main(String[] args) {
        Fz.init("VectorMasks", args);
        int iters = 20_000 * Fz.scale;
        Fz.out("errors", errors());
        int[] src = {10, 20, 30, 40}, idx = {3, 0, 1, 2};
        VectorMask<Integer> gm = VectorMask.fromArray(I128, new boolean[] {true, false, true, false}, 0);
        Fz.out("gather.example", Arrays.toString(IntVector.fromArray(I128, src, 0, idx, 0, gm).toArray()));
        StringBuilder inactive = new StringBuilder();
        for (int bad : BAD) {
            try {
                IntVector g = IntVector.fromArray(I128, src, 0, new int[] {3, bad, 1, bad}, 0, gm);
                inactive.append(Arrays.toString(g.toArray())).append(' ');
            } catch (IndexOutOfBoundsException e) {
                inactive.append("ioobe ");
            }
            try {
                int[] dst = new int[4];
                IntVector.fromArray(I128, src, 0).intoArray(dst, 0, new int[] {3, bad, 1, bad}, 0, gm);
                inactive.append(Arrays.toString(dst)).append(' ');
            } catch (IndexOutOfBoundsException e) {
                inactive.append("ioobe ");
            }
        }
        Fz.out("gather.invalidInactive", inactive.toString().trim());
        IntVector ce = IntVector.fromArray(I128, new int[] {1, 2, 3, 4}, 0);
        VectorMask<Integer> cm = VectorMask.fromArray(I128, new boolean[] {true, false, true, false}, 0);
        Fz.out("compress.example", Arrays.toString(ce.compress(cm).toArray()) + " " + Arrays.toString(ce.compress(cm).expand(cm).toArray()));
        for (VectorSpecies<Integer> s : java.util.List.of(I64, I128, I256)) {
            String k = "i" + s.vectorBitSize();
            long gh = gathers(s, iters), th = tails(s, iters / 4), lh = lanes(s, iters);
            Fz.out(k + ".gather", Long.toHexString(gh));
            Fz.out(k + ".tails", Long.toHexString(th));
            Fz.out(k + ".lanes", Long.toHexString(lh));
            Fz.mix(gh);
            Fz.mix(th);
            Fz.mix(lh);
        }
        for (VectorSpecies<Long> s : java.util.List.of(L128, L256)) {
            long lg = longGathers(s, iters);
            Fz.out("l" + s.vectorBitSize() + ".gather", Long.toHexString(lg));
            Fz.mix(lg);
        }
        long d = doubles(iters);
        Fz.out("d128.lanes", Long.toHexString(d));
        Fz.mix(d);
        Fz.done();
    }
}
