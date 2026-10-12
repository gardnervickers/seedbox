import java.nio.charset.StandardCharsets;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** String intrinsics (compare, indexOf, hash, Latin-1/UTF-16 compaction, coding) and regular expressions. */
public final class Strings {
    static String randomString(int len, boolean wide) {
        StringBuilder sb = new StringBuilder(len);
        for (int i = 0; i < len; i++) {
            int r = Fz.rnd.nextInt(100);
            if (wide && r < 5) sb.append((char) (0x400 + Fz.rnd.nextInt(0x200)));
            else if (wide && r < 7) sb.appendCodePoint(0x1F600 + Fz.rnd.nextInt(64));
            else sb.append((char) ('a' + Fz.rnd.nextInt(6)));
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        Fz.init("Strings", args);
        int n = 2_000;
        String[] s = new String[n];
        for (int i = 0; i < n; i++) s[i] = randomString(1 + Fz.rnd.nextInt(200), (i & 3) == 0);
        long acc = 0;
        for (int round = 0; round < 10 * Fz.scale; round++) {
            for (int i = 0; i < n; i++) {
                String a = s[i], b = s[(i * 7 + round) % n];
                acc = acc * 31 + a.compareTo(b);
                acc = acc * 31 + (a.equals(b) ? 1 : 0) + (a.equalsIgnoreCase(b.toUpperCase()) ? 2 : 0);
                acc = acc * 31 + a.indexOf(b.length() > 3 ? b.substring(0, 3) : b);
                acc = acc * 31 + a.indexOf('c', round % 5) + a.lastIndexOf("ab");
                acc = acc * 31 + a.hashCode() + a.codePointCount(0, a.length());
                acc = acc * 31 + (a.contains("abc") ? 1 : 0) + (a.startsWith("a") ? 2 : 0);
                byte[] u8 = a.getBytes(StandardCharsets.UTF_8);
                byte[] u16 = a.getBytes(StandardCharsets.UTF_16LE);
                byte[] l1 = a.getBytes(StandardCharsets.ISO_8859_1);
                acc = acc * 31 + u8.length + u16.length + l1.length;
                Fz.expect(new String(u8, StandardCharsets.UTF_8).equals(a), "utf8 roundtrip");
                Fz.expect(new String(u16, StandardCharsets.UTF_16LE).equals(a), "utf16 roundtrip");
            }
        }
        Fz.out("intrinsics", acc);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 50_000; i++) {
            sb.append(i % 10).append((char) ('a' + i % 26));
            if (i % 1000 == 0) sb.append('Ж');
            if (sb.length() > 4000) {
                sb.reverse();
                Fz.mix(sb.toString());
                sb.setLength(17);
                sb.insert(3, "xyz").deleteCharAt(5).replace(1, 4, "Q");
            }
        }
        Fz.mix(sb.toString());
        Pattern p = Pattern.compile("(a+)(b|c)\\1|([d-f]{2,})(?=a)|(?<w>e)c*");
        long matches = 0;
        for (int i = 0; i < n; i++) {
            Matcher m = p.matcher(s[i]);
            while (m.find()) matches += m.start() * 7 + m.end() + (m.group("w") != null ? 1 : 0);
        }
        Fz.out("regex", matches);
        Fz.out("split", String.join("|", "a,b,,c,".split(",")) + " " + "x1y22z333".replaceAll("[0-9]+", "#"));
        Fz.out("format", String.format(java.util.Locale.ROOT, "%08.3f|%-6s|%x|%,d|%e|%b", Math.PI, "ab", 255, 1234567, 6.02e23, null));
        Fz.out("case", "İstanbul ß ǅ".toUpperCase(java.util.Locale.ROOT) + " " + "ΣΑΣ".toLowerCase(java.util.Locale.ROOT));
        String big = "x".repeat(1 << 16);
        Fz.out("intern", (big.substring(5).intern() == big.substring(5).intern()) + " " + "lit".equals(new String("lit").intern()));
        Fz.out("strip", "   hi\t ".strip() + "|" + " \n a\n b".lines().map(String::strip).toList() + "|" + "ab".repeat(3));
        Fz.done();
    }
}
