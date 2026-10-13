import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Byte-buffer view VarHandles: every access mode on int, long, float and double views of direct,
 * heap, mapped, offset-by-one and read-only buffers, in both byte orders, at every offset in a
 * 16-byte window. Plain access works at any offset; atomic and ordered modes must reject a
 * misaligned address in Java (IllegalStateException) before any exclusive or LSE instruction can
 * fault. Each mode prints a pattern of outcomes per offset; aligned results are checked against
 * a model, and two threads contend on one aligned direct slot.
 */
public final class BufferAtomics {
    static final String[] MODES = {"get", "set", "getVolatile", "setRelease", "getAcquire", "getOpaque",
        "compareAndSet", "compareAndExchange", "weakCompareAndSetPlain", "getAndSet", "getAndAdd",
        "getAndBitwiseXor"};

    static char code(Throwable t) {
        if (t == null) return 'o';
        if (t instanceof IllegalStateException) return 'M';
        if (t instanceof UnsupportedOperationException && !(t instanceof java.nio.ReadOnlyBufferException)) return 'U';
        if (t instanceof java.nio.ReadOnlyBufferException) return 'R';
        if (t instanceof IndexOutOfBoundsException) return 'B';
        return '?';
    }

    /** One access mode at one byte index, with boxed operands of the view's carrier type. */
    static Object access(VarHandle h, String mode, ByteBuffer b, int i, Object v, Object w) {
        return switch (mode) {
            case "get" -> h.get(b, i);
            case "set" -> { h.set(b, i, v); yield null; }
            case "getVolatile" -> h.getVolatile(b, i);
            case "setRelease" -> { h.setRelease(b, i, v); yield null; }
            case "getAcquire" -> h.getAcquire(b, i);
            case "getOpaque" -> h.getOpaque(b, i);
            case "compareAndSet" -> h.compareAndSet(b, i, v, w);
            case "compareAndExchange" -> h.compareAndExchange(b, i, v, w);
            case "weakCompareAndSetPlain" -> h.weakCompareAndSetPlain(b, i, v, w);
            case "getAndSet" -> h.getAndSet(b, i, v);
            case "getAndAdd" -> h.getAndAdd(b, i, v);
            case "getAndBitwiseXor" -> h.getAndBitwiseXor(b, i, v);
            default -> throw new AssertionError(mode);
        };
    }

    static Object value(Class<?> t, long bits) {
        if (t == int.class) return (int) bits;
        if (t == long.class) return bits;
        if (t == float.class) return Float.intBitsToFloat((int) bits);
        return Double.longBitsToDouble(bits);
    }

    static MappedByteBuffer mapped;

    static Map<String, ByteBuffer> buffers() throws Exception {
        Map<String, ByteBuffer> m = new LinkedHashMap<>();
        m.put("direct", ByteBuffer.allocateDirect(64));
        m.put("heap", ByteBuffer.allocate(64));
        m.put("offset1", ByteBuffer.allocateDirect(80).position(1).slice());
        m.put("aligned8", ByteBuffer.allocateDirect(96).position(3).slice().alignedSlice(8));
        if (mapped == null) {
            Path p = Files.createTempFile("fz-atomics", ".bin");
            try (FileChannel c = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                mapped = c.map(FileChannel.MapMode.READ_WRITE, 0, 4096);
            }
            Files.delete(p);
        }
        m.put("mapped", mapped.slice(4096 - 64, 64));
        m.put("readonly", ByteBuffer.allocateDirect(64).asReadOnlyBuffer());
        return m;
    }

    static void matrix() throws Exception {
        Class<?>[] types = {int.class, long.class, float.class, double.class};
        Class<?>[] arrays = {int[].class, long[].class, float[].class, double[].class};
        long hash = 0;
        int lines = 0;
        for (ByteOrder order : new ByteOrder[] {ByteOrder.nativeOrder(), ByteOrder.BIG_ENDIAN}) {
            String ord = order == ByteOrder.BIG_ENDIAN ? "be" : "ne";
            for (int ti = 0; ti < types.length; ti++) {
                VarHandle h = MethodHandles.byteBufferViewVarHandle(arrays[ti], order);
                int size = types[ti] == int.class || types[ti] == float.class ? 4 : 8;
                for (Map.Entry<String, ByteBuffer> e : buffers().entrySet()) {
                    ByteBuffer b = e.getValue();
                    StringBuilder row = new StringBuilder();
                    for (String mode : MODES) {
                        StringBuilder pat = new StringBuilder();
                        // Offsets 0..15, plus one past the last whole element.
                        for (int i = 0; i <= 16; i++) {
                            int at = i == 16 ? b.capacity() - size + 1 : i;
                            Object v = value(types[ti], Fz.rnd.nextLong()), w = value(types[ti], Fz.rnd.nextLong());
                            Throwable t = null;
                            try {
                                Object r = access(h, mode, b, at, v, w);
                                if (r != null) hash = hash * 31 + r.hashCode();
                            } catch (Throwable x) {
                                t = x;
                            }
                            pat.append(code(t));
                        }
                        row.append(' ').append(pat);
                    }
                    String key = "matrix." + ord + "." + types[ti] + "." + e.getKey();
                    Fz.out(key, row.toString().trim());
                    Fz.mix(row.toString());
                    lines++;
                }
            }
        }
        Fz.out("matrix.modes", String.join(",", MODES));
        Fz.out("matrix.lines", lines);
        Fz.out("matrix.valueHash", Long.toHexString(hash));
        Fz.mix(hash);
    }

    /** Aligned atomic arithmetic checked against a plain model, compiled after warm-up. */
    static void model() {
        VarHandle li = MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.nativeOrder());
        VarHandle ib = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        ByteBuffer b = ByteBuffer.allocateDirect(256);
        long[] lm = new long[16];
        int[] im = new int[16];
        int n = 200_000 * Fz.scale;
        for (int k = 0; k < n; k++) {
            int s = Fz.rnd.nextInt(16);
            long d = Fz.edgeLong();
            switch (Fz.rnd.nextInt(4)) {
                case 0 -> { long old = (long) li.getAndAdd(b, s * 8, d); Fz.expect(old == lm[s], "getAndAdd"); lm[s] += d; }
                case 1 -> {
                    boolean ok = li.compareAndSet(b, s * 8, lm[s], d);
                    Fz.expect(ok, "cas");
                    lm[s] = d;
                }
                case 2 -> { long old = (long) li.getAndBitwiseOr(b, s * 8, d); lm[s] |= d; Fz.mix(old); }
                default -> {
                    // A big-endian int view over the upper half of the buffer.
                    int old = (int) ib.getAndAdd(b, 128 + s * 4, (int) d);
                    Fz.expect(old == im[s], "big-endian getAndAdd");
                    im[s] += (int) d;
                }
            }
        }
        long h = 0;
        for (int s = 0; s < 16; s++) {
            Fz.expect((long) li.getVolatile(b, s * 8) == lm[s], "long slot");
            Fz.expect(b.order(ByteOrder.BIG_ENDIAN).getInt(128 + s * 4) == im[s], "int slot");
            h = h * 31 + lm[s] + im[s];
        }
        Fz.out("model.hash", Long.toHexString(h));
        Fz.mix(h);
    }

    static void contention() throws InterruptedException {
        VarHandle li = MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.nativeOrder());
        ByteBuffer b = ByteBuffer.allocateDirect(64);
        int per = 50_000 * Fz.scale;
        long[] adds = new long[2];
        Thread[] ts = new Thread[2];
        for (int t = 0; t < 2; t++) {
            int id = t;
            long d = 1 + Fz.rnd.nextInt(1000);
            adds[t] = d;
            ts[t] = new Thread(() -> {
                for (int k = 0; k < per; k++) {
                    if ((k & 1) == 0) li.getAndAdd(b, 8, d);
                    else {
                        long cur;
                        do cur = (long) li.getVolatile(b, 8); while (!li.weakCompareAndSet(b, 8, cur, cur + d));
                    }
                }
            });
            ts[t].start();
        }
        Fz.join(ts);
        long total = (long) li.getVolatile(b, 8);
        Fz.out("contention.total", total);
        Fz.expect(total == per * (adds[0] + adds[1]), "contended atomic total");
        Fz.mix(total);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("BufferAtomics", args);
        matrix();
        model();
        contention();
        Fz.done();
    }
}
