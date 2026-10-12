import java.util.Arrays;

/** Auto-vectorized loops, reductions and array intrinsics (copy, fill, mismatch, sort, hash). */
public final class ArraysSimd {
    static void fma(float[] a, float[] b, float[] c, float[] d) { for (int i = 0; i < a.length; i++) d[i] = a[i] * b[i] + c[i]; }
    static void ints(int[] a, int[] b, int[] d) { for (int i = 0; i < a.length; i++) d[i] = (a[i] + b[i]) * 3 ^ (a[i] >>> 2) & ~b[i]; }
    static void longs(long[] a, long[] b, long[] d) { for (int i = 0; i < a.length; i++) d[i] = a[i] * b[i] + (a[i] >> 7) - Math.abs(b[i]); }
    static void bytes(byte[] a, byte[] b, byte[] d) { for (int i = 0; i < a.length; i++) d[i] = (byte) (a[i] + b[i] * 2); }
    static void shorts(short[] a, char[] b, short[] d) { for (int i = 0; i < a.length; i++) d[i] = (short) (a[i] * b[i] - (a[i] >> 1)); }
    static void doubles(double[] a, double[] b, double[] d) { for (int i = 0; i < a.length; i++) d[i] = Math.sqrt(Math.abs(a[i])) + Math.max(a[i], b[i]) * 0.5; }
    static void convert(int[] a, float[] f, long[] l) { for (int i = 0; i < a.length; i++) { f[i] = (float) a[i]; l[i] = (long) (f[i] * 1.5f); } }
    static long sumI(int[] a) { long s = 0; for (int x : a) s += x; return s; }
    static int maxI(int[] a) { int m = Integer.MIN_VALUE; for (int x : a) m = Math.max(m, x); return m; }
    static double sumD(double[] a) { double s = 0; for (double x : a) s += x; return s; }
    static float dot(float[] a, float[] b) { float s = 0; for (int i = 0; i < a.length; i++) s += a[i] * b[i]; return s; }
    static int popc(long[] a) { int c = 0; for (long x : a) c += Long.bitCount(x); return c; }

    public static void main(String[] args) {
        Fz.init("ArraysSimd", args);
        int n = 1000 + Fz.rnd.nextInt(3000);
        float[] fa = new float[n], fb = new float[n], fc = new float[n], fd = new float[n];
        int[] ia = new int[n], ib = new int[n], id = new int[n];
        long[] la = new long[n], lb = new long[n], ld = new long[n];
        byte[] ba = Fz.bytes(n), bb = Fz.bytes(n), bd = new byte[n];
        short[] sa = new short[n], sd = new short[n];
        char[] cb = new char[n];
        double[] da = new double[n], db = new double[n], dd = new double[n];
        float[] cf = new float[n];
        long[] cl = new long[n];
        for (int i = 0; i < n; i++) {
            fa[i] = (float) Fz.rnd.nextGaussian(); fb[i] = i * 0.25f; fc[i] = -i;
            ia[i] = Fz.edgeInt(); ib[i] = Fz.rnd.nextInt();
            la[i] = Fz.edgeLong(); lb[i] = Fz.rnd.nextLong();
            sa[i] = (short) Fz.rnd.nextInt(); cb[i] = (char) Fz.rnd.nextInt();
            da[i] = Fz.edgeDouble(); db[i] = Fz.rnd.nextDouble();
        }
        long first = 0;
        for (int r = 0; r < 200 * Fz.scale; r++) {
            fma(fa, fb, fc, fd); ints(ia, ib, id); longs(la, lb, ld); bytes(ba, bb, bd);
            shorts(sa, cb, sd); doubles(da, db, dd); convert(ia, cf, cl);
            long h = Arrays.hashCode(fd) * 31L + Arrays.hashCode(id) + Arrays.hashCode(ld) * 7 + Arrays.hashCode(bd)
                + Arrays.hashCode(sd) + Arrays.hashCode(dd) + sumI(id) + maxI(ia) + Double.doubleToLongBits(sumD(dd))
                + Float.floatToIntBits(dot(fa, fb)) + popc(ld) + Arrays.hashCode(cf) + Arrays.hashCode(cl);
            if (r == 0) first = h;
            else Fz.expect(h == first, "round " + r + " vector result differs");
        }
        Fz.outHex("vector", first);
        int[] copy = Arrays.copyOf(ia, n);
        for (int k = 0; k < 2000; k++) {
            int src = Fz.rnd.nextInt(n), dst = Fz.rnd.nextInt(n), len = Fz.rnd.nextInt(Math.min(n - src, n - dst) + 1);
            System.arraycopy(copy, src, copy, dst, len);
        }
        Fz.mix(Arrays.hashCode(copy));
        byte[] x = Fz.bytes(5000), y = x.clone();
        y[4321] ^= 1;
        Fz.out("mismatch", Arrays.mismatch(x, y) + " " + Arrays.equals(x, 0, 4321, y, 0, 4321) + " " + Arrays.compare(x, y));
        long[] fill = new long[777];
        Arrays.fill(fill, 3, 700, -5L);
        Fz.mix(Arrays.hashCode(fill));
        int[] sorted = ia.clone();
        Arrays.sort(sorted);
        double[] ds = da.clone();
        Arrays.sort(ds);
        Integer[] boxed = Arrays.stream(ib).boxed().toArray(Integer[]::new);
        Arrays.sort(boxed, (p, q) -> Integer.compare(q & 0xffff, p & 0xffff));
        long[] ps = la.clone();
        Arrays.parallelSort(ps);
        Fz.mix(Arrays.hashCode(sorted)); Fz.mix(Arrays.hashCode(ds)); Fz.mix(Arrays.hashCode(boxed)); Fz.mix(Arrays.hashCode(ps));
        Fz.out("search", Arrays.binarySearch(sorted, ia[7]) >= 0);
        Fz.done();
    }
}
