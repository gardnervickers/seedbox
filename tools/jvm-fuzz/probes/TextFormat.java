import java.math.BigDecimal;
import java.text.Collator;
import java.text.DecimalFormat;
import java.text.DecimalFormatSymbols;
import java.text.Normalizer;
import java.text.NumberFormat;
import java.time.Instant;
import java.time.LocalDate;
import java.time.ZoneId;
import java.time.ZonedDateTime;
import java.time.format.DateTimeFormatter;
import java.util.Arrays;
import java.util.Locale;
import java.util.TreeSet;

/** Locale data, number/date formatting, time-zone rules, collation, normalization and charset tables. */
public final class TextFormat {
    public static void main(String[] args) {
        Fz.init("TextFormat", args);
        Locale[] locales = {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN, Locale.forLanguageTag("ar-EG"), Locale.forLanguageTag("hi-IN")};
        for (Locale l : locales) {
            NumberFormat nf = NumberFormat.getInstance(l);
            NumberFormat cf = NumberFormat.getCurrencyInstance(l);
            DateTimeFormatter df = DateTimeFormatter.ofPattern("EEEE d MMMM uuuu HH:mm", l);
            ZonedDateTime t = Instant.ofEpochSecond(1_700_000_000L + Fz.seed * 86_400).atZone(ZoneId.of("UTC"));
            Fz.out("locale-" + l.toLanguageTag(), nf.format(1234567.891) + " | " + cf.format(-42.5) + " | " + df.format(t));
        }
        String[] zones = {"America/New_York", "Europe/London", "Asia/Kolkata", "Australia/Lord_Howe", "Pacific/Chatham", "America/Sao_Paulo"};
        for (String z : zones) {
            ZoneId id = ZoneId.of(z);
            long h = 0;
            for (int d = 0; d < 365 * 3; d += 1) {
                ZonedDateTime zt = LocalDate.of(2020, 1, 1).plusDays(d).atStartOfDay(id);
                h = h * 31 + zt.getOffset().getTotalSeconds();
            }
            Fz.outHex("zone-" + z, h);
        }
        Collator col = Collator.getInstance(Locale.FRANCE);
        String[] words = {"côte", "cote", "coté", "côté", "Cote", "cotë", "zèbre", "Zebra", "éclair", "eclair"};
        Arrays.sort(words, col);
        Fz.out("collate", String.join(",", words));
        Fz.out("normalize", Normalizer.normalize("éﬁ①", Normalizer.Form.NFKC) + " " + Normalizer.normalize("é", Normalizer.Form.NFD).length());
        DecimalFormat dfmt = new DecimalFormat("#,##0.00;(#)", DecimalFormatSymbols.getInstance(Locale.ROOT));
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 1000; i++) sb.append(dfmt.format((Fz.rnd.nextDouble() - 0.5) * 1e7)).append(' ');
        Fz.mix(sb.toString());
        Fz.out("bigdec", new BigDecimal("1e-7").toPlainString() + " " + new BigDecimal("123.4500").stripTrailingZeros() + " " + Double.toString(0.1 + 0.2) + " " + Float.toString(1.0e-10f));
        Fz.outHex("charsets", new TreeSet<>(java.nio.charset.Charset.availableCharsets().keySet()).hashCode());
        Fz.done();
    }
}
