import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import jdk.jfr.Event;
import jdk.jfr.Label;
import jdk.jfr.Name;
import jdk.jfr.Recording;
import jdk.jfr.StackTrace;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * Custom flight-recorder events only (no sampling): several event types, large string payloads
 * that force chunk rotation (run with -XX:FlightRecorderOptions=maxchunksize=1m), events committed
 * from several threads, a disabled event type, a threshold that discards events, and a second
 * recording that starts while the first rotates. The dump is read back and the application
 * fields are checked; timestamps, thread ids and chunk counts are never printed.
 */
public final class JfrChunks {
    @Name("fz.Text") @Label("Text") @StackTrace(false)
    static final class TextEvent extends Event { int seq; String text; long check; }

    @Name("fz.Numbers") @StackTrace(true)
    static final class NumberEvent extends Event { long a; double b; boolean c; byte d; short e; char f; }

    @Name("fz.Disabled")
    static final class DisabledEvent extends Event { int seq; }

    @Name("fz.Slow") @StackTrace(false)
    static final class SlowEvent extends Event { int seq; }

    static String text(int seq, int len) {
        StringBuilder b = new StringBuilder(len);
        long x = seq * 0x9E3779B97F4A7C15L + Fz.seed;
        for (int i = 0; i < len; i++) {
            x ^= x << 13; x ^= x >>> 7; x ^= x << 17;
            b.append((char) ('a' + (int) ((x >>> 33) % 26)));
            if ((i & 127) == 127) b.append((char) (0x3b1 + (x & 15)));
        }
        return b.toString();
    }

    static long check(String s) {
        long h = 0;
        for (int i = 0; i < s.length(); i++) h = h * 131 + s.charAt(i);
        return h;
    }

    public static void main(String[] args) throws Exception {
        Fz.init("JfrChunks", args);
        int n = 6_000 * Fz.scale;
        int threads = 3;
        Path first = Files.createTempFile("fz-chunks", ".jfr"), second = Files.createTempFile("fz-chunks2", ".jfr");
        long expectSum = 0, expectCheck = 0;
        int numbers = 0;
        try (Recording r = new Recording()) {
            r.enable(TextEvent.class);
            r.enable(NumberEvent.class);
            r.disable(DisabledEvent.class);
            // Nothing in this program takes a day, so every SlowEvent is below the threshold.
            r.enable(SlowEvent.class).withThreshold(java.time.Duration.ofDays(1));
            r.setToDisk(true);
            r.start();
            Recording[] overlap = new Recording[1];
            int[] lens = new int[n];
            for (int i = 0; i < n; i++) lens[i] = 256 + Fz.rnd.nextInt(2048);
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                int id = t;
                ts[t] = new Thread(() -> {
                    for (int i = id; i < n; i += threads) {
                        if (id == 0 && overlap[0] == null && i >= n / 2) {
                            // A second recording starts while this one is rotating chunks.
                            Recording o = new Recording();
                            o.enable(TextEvent.class);
                            o.start();
                            overlap[0] = o;
                        }
                        TextEvent e = new TextEvent();
                        e.begin();
                        e.seq = i;
                        e.text = text(i, lens[i]);
                        e.check = check(e.text);
                        e.commit();
                        if (i % 7 == 0) {
                            NumberEvent ne = new NumberEvent();
                            ne.a = i * 1_000_003L;
                            ne.b = i / 3.0;
                            ne.c = (i & 1) == 0;
                            ne.d = (byte) i;
                            ne.e = (short) (i * 7);
                            ne.f = (char) ('A' + i % 26);
                            ne.commit();
                        }
                        DisabledEvent de = new DisabledEvent();
                        de.seq = i;
                        de.commit();
                        SlowEvent se = new SlowEvent();
                        se.begin();
                        se.seq = i;
                        se.commit();
                    }
                });
            }
            for (Thread t : ts) t.start();
            for (int i = 0; i < n; i++) {
                expectSum += i;
                expectCheck += check(text(i, lens[i]));
                if (i % 7 == 0) numbers++;
            }
            Fz.join(ts);
            r.stop();
            overlap[0].stop();
            overlap[0].dump(second);
            overlap[0].close();
            r.dump(first);
        }
        long count = 0, sum = 0, checks = 0, bad = 0, num = 0, numSum = 0, disabled = 0, slow = 0;
        try (RecordingFile f = new RecordingFile(first)) {
            while (f.hasMoreEvents()) {
                RecordedEvent e = f.readEvent();
                switch (e.getEventType().getName()) {
                    case "fz.Text" -> {
                        count++;
                        int seq = e.getInt("seq");
                        sum += seq;
                        String s = e.getString("text");
                        checks += check(s);
                        if (check(s) != e.getLong("check") || e.getStackTrace() != null) bad++;
                    }
                    case "fz.Numbers" -> {
                        num++;
                        long a = e.getLong("a");
                        int i = (int) (a / 1_000_003L);
                        boolean ok = e.getDouble("b") == i / 3.0 && e.getBoolean("c") == ((i & 1) == 0)
                            && e.getByte("d") == (byte) i && e.getShort("e") == (short) (i * 7)
                            && e.getChar("f") == (char) ('A' + i % 26) && e.getStackTrace() != null;
                        if (!ok) bad++;
                        numSum += i;
                    }
                    case "fz.Disabled" -> disabled++;
                    case "fz.Slow" -> slow++;
                    default -> {}
                }
            }
        }
        List<RecordedEvent> later = RecordingFile.readAllEvents(second);
        long overlapCount = later.stream().filter(e -> e.getEventType().getName().equals("fz.Text")).count();
        Fz.out("text.count", count + "/" + n);
        Fz.out("text.seqSum", sum == expectSum);
        Fz.out("text.contentMatches", checks == expectCheck);
        Fz.out("numbers", num + "/" + numbers);
        Fz.out("badFields", bad);
        Fz.out("disabledRecorded", disabled);
        Fz.out("belowThresholdRecorded", slow);
        Fz.out("overlapHasEvents", overlapCount > 0 && overlapCount <= n);
        Fz.out("fileLargerThanChunk", Files.size(first) > (1 << 20));
        Fz.expect(count == n && bad == 0 && num == numbers && disabled == 0 && slow == 0, "recorded events");
        Fz.mix(sum);
        Fz.mix(checks);
        Fz.mix(numSum);
        Files.deleteIfExists(first);
        Files.deleteIfExists(second);
        Fz.done();
    }
}
