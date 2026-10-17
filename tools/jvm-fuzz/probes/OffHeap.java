import java.lang.reflect.Field;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.LongBuffer;
import sun.misc.Unsafe;

/** Direct buffers, byte-order views, Unsafe raw memory and off-heap CAS. */
public final class OffHeap {
    public static void main(String[] args) throws Exception {
        Fz.init("OffHeap", args);
        long acc = 0;
        for (int round = 0; round < 200 * Fz.scale; round++) {
            ByteBuffer bb = ByteBuffer.allocateDirect(64 * 1024 + round).order((round & 1) == 0 ? ByteOrder.LITTLE_ENDIAN : ByteOrder.BIG_ENDIAN);
            for (int i = 0; i + 8 <= bb.capacity(); i += 8) bb.putLong(i, i * 0x9E3779B97F4A7C15L);
            LongBuffer lb = bb.asLongBuffer();
            for (int i = 0; i < lb.limit(); i += 61) acc = acc * 31 + lb.get(i);
            acc += bb.getInt(13) + bb.getShort(7) + bb.getDouble(21) > 0 ? 1 : 0;
        }
        Fz.out("direct", acc);
        Field f = Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Unsafe u = (Unsafe) f.get(null);
        long size = 1 << 20;
        long base = u.allocateMemory(size);
        u.setMemory(base, size, (byte) 0x5a);
        for (long i = 0; i < size; i += 4096) u.putLong(base + i, i);
        long other = u.allocateMemory(size);
        u.copyMemory(base, other, size);
        long s = 0;
        for (long i = 0; i < size; i += 8) s = s * 31 + u.getLong(other + i);
        Fz.outHex("unsafe", s);
        Thread[] ts = new Thread[3];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 100_000; i++) {
                    long v;
                    do { v = u.getLongVolatile(null, base); } while (!u.compareAndSwapLong(null, base, v, v + 1));
                    u.getAndAddInt(null, base + 64, 2);
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        Fz.out("cas", u.getLong(base) + " " + u.getInt(base + 64));
        u.freeMemory(base);
        u.freeMemory(other);
        Fz.done();
    }
}
