import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.MathContext;
import java.math.RoundingMode;

/** Arbitrary-precision integer and decimal arithmetic. */
public final class BigNumbers {
    static BigDecimal arctanInv(int x, MathContext mc) {
        BigDecimal result = BigDecimal.ZERO, xs = BigDecimal.valueOf((long) x * x), term = BigDecimal.ONE.divide(BigDecimal.valueOf(x), mc);
        for (int k = 0; term.compareTo(BigDecimal.ZERO) != 0 && k < 5000; k++) {
            BigDecimal t = term.divide(BigDecimal.valueOf(2L * k + 1), mc);
            result = (k & 1) == 0 ? result.add(t, mc) : result.subtract(t, mc);
            term = term.divide(xs, mc);
        }
        return result;
    }

    public static void main(String[] args) {
        Fz.init("BigNumbers", args);
        BigInteger f = BigInteger.ONE;
        for (int i = 2; i <= 1000 * Fz.scale; i++) f = f.multiply(BigInteger.valueOf(i));
        Fz.outHex("factorial", f.hashCode());
        Fz.out("factorialDigits", f.toString().length());
        MathContext mc = new MathContext(300 + 100 * Fz.scale);
        BigDecimal pi = arctanInv(5, mc).multiply(BigDecimal.valueOf(16)).subtract(arctanInv(239, mc).multiply(BigDecimal.valueOf(4)), mc);
        Fz.out("pi", pi.round(new MathContext(60)));
        BigInteger a = new BigInteger(1, Fz.bytes(512)), b = new BigInteger(1, Fz.bytes(300));
        Fz.outHex("gcd", a.gcd(b).hashCode());
        BigInteger[] qr = a.divideAndRemainder(b);
        Fz.expect(qr[0].multiply(b).add(qr[1]).equals(a), "divmod");
        Fz.outHex("divide", qr[0].hashCode() ^ qr[1].hashCode());
        Fz.outHex("sqrt", a.sqrt().hashCode());
        Fz.outHex("bitops", a.xor(b).andNot(b.shiftLeft(13)).bitCount() + a.bitLength() * 31L);
        BigDecimal x = new BigDecimal("2");
        Fz.out("sqrt2", x.sqrt(new MathContext(50)));
        Fz.out("rounding", new BigDecimal("2.5").setScale(0, RoundingMode.HALF_EVEN) + " " + new BigDecimal("-2.5").setScale(0, RoundingMode.HALF_UP) + " "
            + BigDecimal.ONE.divide(BigDecimal.valueOf(7), 30, RoundingMode.DOWN));
        Fz.done();
    }
}
