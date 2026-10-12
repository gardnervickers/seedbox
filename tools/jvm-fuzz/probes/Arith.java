/** Integer, long and floating-point edge semantics, checked across interpreter and JIT tiers. */
public final class Arith {
    static int iop(int op, int a, int b) {
        switch (op) {
            case 0: return a + b;
            case 1: return a - b;
            case 2: return a * b;
            case 3: return b == 0 ? 0 : a / b;
            case 4: return b == 0 ? 0 : a % b;
            case 5: return a << b;
            case 6: return a >> b;
            case 7: return a >>> b;
            case 8: return a & b | (a ^ ~b);
            case 9: return Math.floorDiv(a, b == 0 ? 3 : b) + Math.floorMod(a, b == 0 ? 5 : b);
            case 10: return Integer.rotateLeft(a, b) ^ Integer.rotateRight(b, a);
            case 11: return Integer.bitCount(a) + Integer.numberOfLeadingZeros(b) * 3 + Integer.numberOfTrailingZeros(a ^ b);
            case 12: return Integer.reverse(a) ^ Integer.reverseBytes(b);
            case 13: return Integer.compareUnsigned(a, b) + (b == 0 ? 0 : Integer.divideUnsigned(a, b) ^ Integer.remainderUnsigned(a, b));
            case 14: return Math.abs(a) + Math.max(a, b) - Math.min(a, b);
            case 15: return a / 7 + a % 10 + (a >>> 3) / 5 - a / -3 + a * 0x9E3779B9;
            case 16: return (short) a + (byte) b + (char) (a ^ b);
            case 17: return Integer.compress(a, b) ^ Integer.expand(a, b);
            default: return Integer.highestOneBit(a) ^ Integer.lowestOneBit(b) ^ Integer.signum(a - b);
        }
    }

    static long lop(int op, long a, long b) {
        switch (op) {
            case 0: return a + b;
            case 1: return a - b * 3;
            case 2: return a * b;
            case 3: return b == 0 ? 0 : a / b;
            case 4: return b == 0 ? 0 : a % b;
            case 5: return a << b ^ a >> b ^ a >>> b;
            case 6: return Math.multiplyHigh(a, b) ^ Math.unsignedMultiplyHigh(a, b);
            case 7: return Long.rotateLeft(a, (int) b) + Long.bitCount(b) + Long.numberOfLeadingZeros(a);
            case 8: return Long.reverse(a) ^ Long.reverseBytes(b);
            case 9: return b == 0 ? 1 : Long.divideUnsigned(a, b) + Long.remainderUnsigned(a, b);
            case 10: return Math.floorDiv(a, b == 0 ? 7 : b) ^ Math.floorMod(a, b == 0 ? 9 : b);
            case 11: return a / 1000000007L + a % 10L + a / -17L;
            case 12: return (int) a + (long) (int) b;
            case 13: return Long.compress(a, b) + Long.expand(b, a);
            default: return Math.abs(a) ^ Math.max(a, b) ^ Math.min(a, b);
        }
    }

    static double dop(int op, double a, double b) {
        switch (op) {
            case 0: return a + b;
            case 1: return a - b;
            case 2: return a * b;
            case 3: return a / b;
            case 4: return a % b;
            case 5: return Math.sqrt(Math.abs(a)) + Math.cbrt(b);
            case 6: return Math.fma(a, b, a);
            case 7: return Math.floor(a) + Math.ceil(b) + Math.rint(a * 0.5);
            case 8: return Math.sin(a) + Math.cos(b) + Math.tan(a * 1e-3);
            case 9: return Math.exp(a * 1e-3) + Math.log(Math.abs(b)) + Math.log10(Math.abs(a));
            case 10: return Math.pow(a, b * 1e-2) + Math.atan2(a, b) + Math.hypot(a, b);
            case 11: return Math.copySign(a, b) + Math.scalb(a, (int) b % 64) + Math.ulp(b);
            case 12: return (int) a + (long) b + (float) a + (double) (float) b;
            case 13: return Math.min(a, b) - Math.max(a, b) + Math.abs(a) + Math.signum(b);
            case 14: return StrictMath.sin(a) + StrictMath.exp(b * 1e-3) + StrictMath.log1p(Math.abs(a));
            case 15: return Math.round(a) + Math.round((float) b) + Math.toIntExact(Math.round(Math.tanh(a) * 1000));
            default: return (float) a * (float) b + Float.intBitsToFloat(Float.floatToRawIntBits((float) a));
        }
    }

    public static void main(String[] args) {
        Fz.init("Arith", args);
        int n = 4096;
        int[] io = new int[n], ia = new int[n], ib = new int[n];
        long[] la = new long[n], lb = new long[n];
        double[] da = new double[n], db = new double[n];
        for (int i = 0; i < n; i++) {
            io[i] = Fz.rnd.nextInt(19);
            ia[i] = Fz.edgeInt();
            ib[i] = Fz.edgeInt();
            la[i] = Fz.edgeLong();
            lb[i] = Fz.edgeLong();
            da[i] = Fz.edgeDouble();
            db[i] = Fz.edgeDouble();
        }
        long first = 0;
        int rounds = 3 + 4 * Fz.scale;
        for (int r = 0; r < rounds; r++) {
            long h = 17;
            for (int i = 0; i < n; i++) {
                h = h * 31 + iop(io[i], ia[i], ib[i]);
                h = h * 31 + lop(io[i] % 15, la[i], lb[i]);
                h = h * 31 + Double.doubleToLongBits(dop(io[i] % 17, da[i], db[i]));
            }
            if (r == 0) {
                first = h;
                Fz.outHex("round0", h);
            } else {
                Fz.expect(h == first, "round " + r + " differs from round 0: " + Long.toHexString(h));
            }
        }
        Fz.mix(first);
        try {
            Fz.mix(Math.addExact(Integer.MAX_VALUE, 1));
        } catch (ArithmeticException e) {
            Fz.mix(e.getMessage());
        }
        try {
            Fz.mix(Math.multiplyExact(Long.MIN_VALUE, -1L));
        } catch (ArithmeticException e) {
            Fz.mix(e.getMessage());
        }
        Fz.out("minDivNeg1", Integer.MIN_VALUE / -1 + " " + Long.MIN_VALUE / -1L + " " + Integer.MIN_VALUE % -1);
        Fz.out("nanCast", (int) Double.NaN + " " + (long) Float.POSITIVE_INFINITY + " " + (int) -1e30);
        Fz.done();
    }
}
