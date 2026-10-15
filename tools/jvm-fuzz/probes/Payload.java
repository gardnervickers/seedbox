/** Loaded repeatedly by ClassLoading through fresh loaders; each copy gets its own compiled code. */
public final class Payload implements java.util.function.LongUnaryOperator {
    static long calls;
    private final long k;

    public Payload() { this.k = 0x9E3779B97F4A7C15L; }

    public long applyAsLong(long x) {
        calls++;
        long h = x ^ k;
        for (int i = 0; i < 8; i++) {
            h ^= h >>> 31;
            h *= 0xBF58476D1CE4E5B9L;
            h += i;
        }
        return h;
    }
}
