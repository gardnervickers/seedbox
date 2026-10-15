/**
 * Many floating-point values live across a loop body (C2 allocates v8-v31 and spills to
 * vector registers): fma, sqrt, rint, copySign, signum and half-precision conversion. Every
 * round must equal round 0.
 */
public final class FpRegPressure {
    static long kernel(int n, double seed) {
        double a0 = seed, a1 = seed + 1, a2 = seed * 2, a3 = seed - 3, a4 = 0.5, a5 = 1.5, a6 = -2.5, a7 = 3.25;
        double a8 = 1e-3, a9 = 7, a10 = 11, a11 = 13, a12 = 0.1, a13 = 0.2, a14 = 0.3, a15 = 0.4;
        double a16 = 17, a17 = 19, a18 = 23, a19 = 29, a20 = 31, a21 = 37, a22 = 41, a23 = 43;
        float f0 = 1f, f1 = 2f, f2 = 3f, f3 = 4f, f4 = 5f, f5 = 6f, f6 = 7f, f7 = 8f;
        float f8 = 9f, f9 = 10f, f10 = 11f, f11 = 12f, f12 = 13f, f13 = 14f, f14 = 15f, f15 = 16f;
        for (int i = 0; i < n; i++) {
            a0 = Math.fma(a0, 0.999, a1 * 1e-6); a1 = Math.sqrt(Math.abs(a1 + a2)); a2 = Math.rint(a2 * 1.0001 + a3);
            a3 = Math.copySign(a3 + a4, a5); a4 = a4 * 0.99 + Math.signum(a6) * 0.01; a5 = Math.fma(a5, a6, a7) * 1e-3;
            a6 = a6 + a8; a7 = a7 * 0.5 + a9 * 0.5; a8 = Math.abs(a8 - a10 * 1e-7); a9 = a9 + a11 * 1e-9;
            a10 = Math.max(a10, a12); a11 = Math.min(a11, a13 + 10); a12 = a12 + a14 * a15; a13 = a13 * 0.9 + 0.01;
            a14 = Math.sqrt(a14 * a14 + 1e-9); a15 = Math.fma(a15, 0.5, a16 * 1e-4); a16 = a16 + a17 * 1e-5;
            a17 = a17 * 0.999 + a18 * 0.001; a18 = Math.rint(a18 + 0.25); a19 = a19 - a20 * 1e-6; a20 = a20 * 1.0000001;
            a21 = a21 + Math.signum(a22) * 1e-3; a22 = a22 * 0.5 + a23 * 0.5; a23 = Math.copySign(a23, -a0);
            f0 = Math.fma(f0, 0.5f, f1); f1 = (float) Math.sqrt(f1 + f2); f2 = Math.max(f2, f3) * 0.999f; f3 = f3 + f4 * 1e-3f;
            f4 = Float.float16ToFloat(Float.floatToFloat16(f4 + f5 * 1e-3f)); f5 = f5 * 0.99f + f6 * 0.01f;
            f6 = Math.abs(f6 - f7); f7 = f7 + 0.5f; f8 = Math.min(f8, f9 + 1); f9 = f9 * 0.5f + f10 * 0.5f;
            f10 = f10 + f11 * 1e-4f; f11 = Math.copySign(f11, f12 - 13.5f); f12 = f12 + Math.signum(f13) * 1e-3f;
            f13 = f13 * 0.9f + 1; f14 = Math.fma(f14, 1.0001f, f15 * 1e-6f); f15 = (float) Math.rint(f15 + 0.25f);
        }
        double d = a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 + a10 + a11 + a12 + a13 + a14 + a15
            + a16 + a17 + a18 + a19 + a20 + a21 + a22 + a23;
        float f = f0 + f1 + f2 + f3 + f4 + f5 + f6 + f7 + f8 + f9 + f10 + f11 + f12 + f13 + f14 + f15;
        return Double.doubleToLongBits(d) * 31 + Float.floatToIntBits(f);
    }

    public static void main(String[] args) {
        Fz.init("FpRegPressure", args);
        double seed = Fz.rnd.nextDouble() * 10;
        long first = 0;
        for (int r = 0; r < 30 * Fz.scale; r++) {
            long h = kernel(50_000, seed);
            if (r == 0) first = h;
            else Fz.expect(h == first, "round " + r + " differs: " + Long.toHexString(h));
        }
        Fz.outHex("kernel", first);
        Fz.done();
    }
}
