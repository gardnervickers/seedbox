import java.util.Arrays;
import java.util.Base64;

/**
 * HotSpot's hand-written copy, fill, compare and encoding stubs: System.arraycopy for every
 * element type with overlapping ranges and odd offsets, checked Object[] copies that fail part
 * way, large clones and fills, Arrays.equals/mismatch and Base64. Rounds must agree.
 */
public final class ArrayCopyStubs {
    static long round(int seedish) {
        long h = 17;
        byte[] b = new byte[4096 + 70];
        short[] s = new short[2048 + 70];
        int[] i32 = new int[1024 + 70];
        long[] i64 = new long[512 + 70];
        char[] c = new char[2048 + 70];
        for (int k = 0; k < b.length; k++) b[k] = (byte) (k * 31 + seedish);
        for (int k = 0; k < s.length; k++) s[k] = (short) (k * 7 + seedish);
        for (int k = 0; k < i32.length; k++) i32[k] = k * 0x9E3779B9 + seedish;
        for (int k = 0; k < i64.length; k++) i64[k] = k * 0x9E3779B97F4A7C15L;
        for (int k = 0; k < c.length; k++) c[k] = (char) (k % 300);
        for (int len = 0; len <= 70; len++) {
            for (int off : new int[] {0, 1, 3, 5, 7}) {
                System.arraycopy(b, off, b, off + 1 + (len & 3), len);
                System.arraycopy(b, off + 2, b, off, len);
                System.arraycopy(s, off, s, off + 3, len);
                System.arraycopy(i32, off + 1, i32, off, len);
                System.arraycopy(i64, off, i64, off + 2, len);
                System.arraycopy(c, off + 5, c, off, len);
            }
        }
        h = h * 31 + Arrays.hashCode(b) + Arrays.hashCode(s) + Arrays.hashCode(i32) + Arrays.hashCode(i64) + Arrays.hashCode(c);
        Object[] src = new Object[64];
        for (int k = 0; k < src.length; k++) src[k] = (k == 40) ? Integer.valueOf(k) : "s" + k;
        String[] dst = new String[64];
        try {
            System.arraycopy(src, 0, dst, 0, src.length);
        } catch (ArrayStoreException e) {
            int copied = 0;
            while (copied < dst.length && dst[copied] != null) copied++;
            h = h * 31 + copied;
        }
        long[] big = new long[1 << 16];
        Arrays.fill(big, seedish);
        long[] cl = big.clone();
        cl[777] ^= 1;
        h = h * 31 + Arrays.mismatch(big, cl) + (Arrays.equals(big, cl) ? 1 : 0);
        byte[] zero = new byte[100_000];
        h = h * 31 + Arrays.hashCode(zero);
        byte[] enc = Base64.getEncoder().encode(Arrays.copyOf(b, 3001));
        byte[] dec = Base64.getDecoder().decode(enc);
        h = h * 31 + Arrays.hashCode(enc) + (Arrays.equals(dec, Arrays.copyOf(b, 3001)) ? 1 : 0);
        String latin = new String(b, 0, 2000, java.nio.charset.StandardCharsets.ISO_8859_1);
        String utf16 = new String(c, 0, 2000);
        h = h * 31 + latin.indexOf("\u0010/") + utf16.indexOf((char) 299) + latin.compareTo(utf16);
        return h;
    }

    public static void main(String[] args) {
        Fz.init("ArrayCopyStubs", args);
        int seedish = Fz.rnd.nextInt(1000);
        long first = 0;
        for (int r = 0; r < 200 * Fz.scale; r++) {
            long h = round(seedish);
            if (r == 0) first = h;
            else Fz.expect(h == first, "round " + r + " differs");
        }
        Fz.outHex("stubs", first);
        Fz.done();
    }
}
