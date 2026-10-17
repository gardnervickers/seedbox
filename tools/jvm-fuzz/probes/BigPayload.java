/** Loaded repeatedly by CodeReuse; inlining its steps makes a large compiled method. */
public final class BigPayload implements java.util.function.LongUnaryOperator {
    private static long s0(long h) { h ^= h >>> 13; h *= 0x1e3779b97f4a7c15L; h += 0; return Long.rotateLeft(h, 7); }
    private static long s1(long h) { h ^= h >>> 14; h *= 0x1e3779b97e4a7d86L; h += 1; return Long.rotateLeft(h, 8); }
    private static long s2(long h) { h ^= h >>> 15; h *= 0x1e3779b97d4a7f33L; h += 2; return Long.rotateLeft(h, 9); }
    private static long s3(long h) { h ^= h >>> 16; h *= 0x1e3779b97c4a78acL; h += 3; return Long.rotateLeft(h, 10); }
    private static long s4(long h) { h ^= h >>> 17; h *= 0x1e3779b97b4a7a59L; h += 4; return Long.rotateLeft(h, 11); }
    private static long s5(long h) { h ^= h >>> 18; h *= 0x1e3779b97a4a7bcaL; h += 5; return Long.rotateLeft(h, 12); }
    private static long s6(long h) { h ^= h >>> 19; h *= 0x1e3779b9794a7567L; h += 6; return Long.rotateLeft(h, 13); }
    private static long s7(long h) { h ^= h >>> 20; h *= 0x1e3779b9784a7710L; h += 7; return Long.rotateLeft(h, 14); }
    private static long s8(long h) { h ^= h >>> 21; h *= 0x1e3779b9774a708dL; h += 8; return Long.rotateLeft(h, 15); }
    private static long s9(long h) { h ^= h >>> 22; h *= 0x1e3779b9764a723eL; h += 9; return Long.rotateLeft(h, 16); }
    private static long s10(long h) { h ^= h >>> 23; h *= 0x1e3779b9754a73abL; h += 10; return Long.rotateLeft(h, 17); }
    private static long s11(long h) { h ^= h >>> 24; h *= 0x1e3779b9744a6d44L; h += 11; return Long.rotateLeft(h, 18); }
    private static long s12(long h) { h ^= h >>> 25; h *= 0x1e3779b9734a6ef1L; h += 12; return Long.rotateLeft(h, 19); }
    private static long s13(long h) { h ^= h >>> 26; h *= 0x1e3779b9724a6862L; h += 13; return Long.rotateLeft(h, 20); }
    private static long s14(long h) { h ^= h >>> 27; h *= 0x1e3779b9714a6a1fL; h += 14; return Long.rotateLeft(h, 21); }
    private static long s15(long h) { h ^= h >>> 28; h *= 0x1e3779b9704a6b88L; h += 15; return Long.rotateLeft(h, 22); }
    private static long s16(long h) { h ^= h >>> 29; h *= 0x1e3779b96f4a6525L; h += 16; return Long.rotateLeft(h, 23); }
    private static long s17(long h) { h ^= h >>> 30; h *= 0x1e3779b96e4a66d6L; h += 17; return Long.rotateLeft(h, 24); }
    private static long s18(long h) { h ^= h >>> 31; h *= 0x1e3779b96d4a6043L; h += 18; return Long.rotateLeft(h, 25); }
    private static long s19(long h) { h ^= h >>> 32; h *= 0x1e3779b96c4a61fcL; h += 19; return Long.rotateLeft(h, 26); }
    private static long s20(long h) { h ^= h >>> 33; h *= 0x1e3779b96b4a6369L; h += 20; return Long.rotateLeft(h, 27); }
    private static long s21(long h) { h ^= h >>> 34; h *= 0x1e3779b96a4a5d1aL; h += 21; return Long.rotateLeft(h, 28); }
    private static long s22(long h) { h ^= h >>> 35; h *= 0x1e3779b9694a5eb7L; h += 22; return Long.rotateLeft(h, 29); }
    private static long s23(long h) { h ^= h >>> 36; h *= 0x1e3779b9684a5820L; h += 23; return Long.rotateLeft(h, 7); }
    public long applyAsLong(long x) {
        long h = x;
        h = s0(h);
        h = s1(h);
        h = s2(h);
        h = s3(h);
        h = s4(h);
        h = s5(h);
        h = s6(h);
        h = s7(h);
        h = s8(h);
        h = s9(h);
        h = s10(h);
        h = s11(h);
        h = s12(h);
        h = s13(h);
        h = s14(h);
        h = s15(h);
        h = s16(h);
        h = s17(h);
        h = s18(h);
        h = s19(h);
        h = s20(h);
        h = s21(h);
        h = s22(h);
        h = s23(h);
        return h;
    }
}
