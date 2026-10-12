import java.time.*;
import java.time.format.DateTimeFormatter;
import java.time.temporal.ChronoUnit;

/** Clock reads, sleeps and timed waits; only invariants are printed. */
public final class Time {
    public static void main(String[] args) throws Exception {
        Fz.init("Time", args);
        long prev = System.nanoTime();
        boolean mono = true;
        for (int i = 0; i < 1_000_000 * Fz.scale; i++) {
            long now = System.nanoTime();
            mono &= now >= prev;
            prev = now;
        }
        Fz.out("nanoMonotonic", mono);
        boolean slept = true;
        for (int ms : new int[] {1, 2, 5, 10, 20, 50}) {
            long t0 = System.nanoTime();
            Thread.sleep(ms);
            slept &= System.nanoTime() - t0 >= ms * 1_000_000L;
        }
        Fz.out("sleepAtLeast", slept);
        Object o = new Object();
        long t0 = System.nanoTime();
        synchronized (o) { o.wait(30); }
        Fz.out("waitAtLeast", System.nanoTime() - t0 >= 29_000_000L);
        long wall = System.currentTimeMillis();
        Instant inst = Instant.now();
        Fz.out("wallAfter2020", wall > 1_577_836_800_000L && inst.isAfter(Instant.parse("2020-01-01T00:00:00Z")));
        Fz.out("wallVsInstant", Math.abs(inst.toEpochMilli() - wall) < 10_000);
        ZonedDateTime fixed = ZonedDateTime.of(2024, 2, 29, 23, 59, 59, 999_999_999, ZoneId.of("Europe/Berlin"));
        Fz.out("fixed", DateTimeFormatter.ISO_OFFSET_DATE_TIME.format(fixed.plusNanos(1).plus(Period.ofMonths(13))) + " "
            + ChronoUnit.DAYS.between(LocalDate.of(1900, 1, 1), LocalDate.of(2100, 12, 31)) + " " + Duration.ofSeconds(-3, 7));
        Fz.done();
    }
}
