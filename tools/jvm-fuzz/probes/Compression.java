import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.util.zip.*;

/** Native zlib through Deflater/Inflater, GZIP and ZIP streams. */
public final class Compression {
    static byte[] text(int n) {
        String[] words = {"seed", "box", "jvm", "deterministic", "replay", "kafka", "broker", "the", "a", "of"};
        StringBuilder sb = new StringBuilder();
        while (sb.length() < n) sb.append(words[Fz.rnd.nextInt(words.length)]).append(Fz.rnd.nextInt(10) == 0 ? '\n' : ' ');
        return sb.toString().getBytes();
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Compression", args);
        byte[] data = text(200_000 * Fz.scale);
        byte[] noise = Fz.bytes(50_000);
        for (int level = 0; level <= 9; level += 3) {
            for (int strategy : new int[] {Deflater.DEFAULT_STRATEGY, Deflater.FILTERED, Deflater.HUFFMAN_ONLY}) {
                Deflater d = new Deflater(level, (level & 1) == 1);
                d.setStrategy(strategy);
                d.setInput(data);
                d.finish();
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                byte[] buf = new byte[4096];
                while (!d.finished()) out.write(buf, 0, d.deflate(buf, 0, buf.length, Deflater.NO_FLUSH));
                d.end();
                byte[] comp = out.toByteArray();
                Inflater inf = new Inflater((level & 1) == 1);
                inf.setInput(comp);
                byte[] back = new byte[data.length];
                int got = 0;
                while (!inf.finished()) got += inf.inflate(back, got, back.length - got);
                inf.end();
                Fz.expect(got == data.length && java.util.Arrays.equals(back, data), "inflate " + level);
                Fz.out("deflate" + level + "s" + strategy, comp.length + " " + Integer.toHexString(java.util.Arrays.hashCode(comp)));
            }
        }
        ByteArrayOutputStream gz = new ByteArrayOutputStream();
        try (GZIPOutputStream g = new GZIPOutputStream(gz, 1024, true)) { g.write(data); g.flush(); g.write(noise); }
        try (GZIPInputStream g = new GZIPInputStream(new ByteArrayInputStream(gz.toByteArray()))) {
            byte[] all = g.readAllBytes();
            Fz.expect(all.length == data.length + noise.length, "gzip length");
            Fz.mix(all);
        }
        Fz.out("gzip", gz.size());
        ByteArrayOutputStream zb = new ByteArrayOutputStream();
        try (ZipOutputStream z = new ZipOutputStream(zb)) {
            for (int i = 0; i < 20; i++) {
                ZipEntry e = new ZipEntry("dir/file" + i + ".txt");
                e.setTime(1_000_000_000_000L);
                e.setMethod(i % 3 == 0 ? ZipEntry.DEFLATED : ZipEntry.DEFLATED);
                z.putNextEntry(e);
                z.write(data, i * 100, 5000 + i);
                z.closeEntry();
            }
        }
        long entries = 0;
        try (ZipInputStream z = new ZipInputStream(new ByteArrayInputStream(zb.toByteArray()))) {
            ZipEntry e;
            while ((e = z.getNextEntry()) != null) entries += e.getName().length() * 31L + z.readAllBytes().length;
        }
        Fz.out("zip", zb.size() + " " + entries);
        Fz.done();
    }
}
