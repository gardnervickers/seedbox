import com.sun.management.HotSpotDiagnosticMXBean;
import java.io.BufferedInputStream;
import java.io.DataInputStream;
import java.io.EOFException;
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * VM-internal heap and native-memory walks. HotSpotDiagnosticMXBean.dumpHeap writes HPROF files
 * (live and all objects), which are parsed here: every Marker instance, its fields and the byte
 * array it references must come back with the values the program stored. With
 * -XX:NativeMemoryTracking=detail the diagnostic-command MBean runs VM.native_memory summary,
 * detail, baseline and diffs, walking malloc sites with native stacks and the virtual-memory map.
 */
public final class HeapNmt {
    static final class Marker {
        int v;
        long w;
        byte[] ref;
        Marker next;

        Marker(int v, long w, byte[] ref, Marker next) {
            this.v = v;
            this.w = w;
            this.ref = ref;
            this.next = next;
        }
    }

    static Marker[] keep;

    /** What a dump says about Marker objects. */
    static final class Found {
        int idSize;
        Map<Long, String> strings = new HashMap<>();
        Map<Long, Long> classNames = new HashMap<>();   // class object id -> name string id
        long markerClass = -1;
        List<long[]> markerFields = new ArrayList<>();  // per field: {name id, type}
        List<byte[]> markerInstances = new ArrayList<>();
        Map<Long, Integer> byteArrays = new HashMap<>(); // id -> Arrays.hashCode
        Map<Long, Long> markerIds = new HashMap<>();     // instance id -> index in markerInstances
        int records, heapSegments, threadRoots;
    }

    static long id(DataInputStream in, int size) throws IOException {
        return size == 8 ? in.readLong() : in.readInt() & 0xffffffffL;
    }

    static int typeSize(int type, int idSize) {
        return switch (type) {
            case 2 -> idSize;
            case 4, 8 -> 1;
            case 5, 9 -> 2;
            case 6, 10 -> 4;
            case 7, 11 -> 8;
            default -> throw new IllegalStateException("hprof type " + type);
        };
    }

    static void skip(DataInputStream in, long n) throws IOException {
        while (n > 0) {
            long k = in.skip(n);
            if (k <= 0) {
                in.readByte();
                k = 1;
            }
            n -= k;
        }
    }

    static Found parse(Path path) throws IOException {
        Found f = new Found();
        try (DataInputStream in = new DataInputStream(new BufferedInputStream(Files.newInputStream(path), 1 << 16))) {
            StringBuilder header = new StringBuilder();
            for (int c; (c = in.read()) != 0; ) header.append((char) c);
            Fz.expect(header.toString().equals("JAVA PROFILE 1.0.2"), "hprof header " + header);
            f.idSize = in.readInt();
            in.readLong();
            int ids = f.idSize;
            while (true) {
                int tag;
                try {
                    tag = in.readUnsignedByte();
                } catch (EOFException e) {
                    break;
                }
                in.readInt();
                long len = in.readInt() & 0xffffffffL;
                f.records++;
                if (tag == 0x01) {
                    long sid = id(in, ids);
                    byte[] b = new byte[(int) len - ids];
                    in.readFully(b);
                    String s = new String(b, StandardCharsets.UTF_8);
                    if (s.equals("HeapNmt$Marker") || s.equals("v") || s.equals("w") || s.equals("ref") || s.equals("next"))
                        f.strings.put(sid, s);
                } else if (tag == 0x02) {
                    in.readInt();
                    long cid = id(in, ids);
                    in.readInt();
                    long nameId = id(in, ids);
                    if ("HeapNmt$Marker".equals(f.strings.get(nameId))) f.markerClass = cid;
                } else if (tag == 0x0C || tag == 0x1C) {
                    f.heapSegments++;
                    segment(in, len, f);
                } else {
                    skip(in, len);
                }
            }
        }
        return f;
    }

    static void segment(DataInputStream in, long len, Found f) throws IOException {
        int ids = f.idSize;
        long end = len;
        long[] used = {0};
        // Counts bytes consumed so the segment end is exact.
        while (used[0] < end) {
            int sub = in.readUnsignedByte();
            used[0]++;
            switch (sub) {
                case 0xFF, 0x05, 0x07 -> { id(in, ids); used[0] += ids; }
                case 0x01 -> { id(in, ids); id(in, ids); used[0] += 2L * ids; }
                case 0x02, 0x03, 0x08 -> {
                    id(in, ids); in.readInt(); in.readInt(); used[0] += ids + 8;
                    if (sub == 0x08) f.threadRoots++;
                }
                case 0x04, 0x06 -> { id(in, ids); in.readInt(); used[0] += ids + 4; }
                case 0x20 -> {
                    long cid = id(in, ids);
                    in.readInt();
                    for (int i = 0; i < 6; i++) id(in, ids);
                    in.readInt();
                    used[0] += 7L * ids + 8;
                    int cp = in.readUnsignedShort();
                    used[0] += 2;
                    for (int i = 0; i < cp; i++) {
                        in.readUnsignedShort();
                        int t = in.readUnsignedByte();
                        int s = typeSize(t, ids);
                        skip(in, s);
                        used[0] += 3 + s;
                    }
                    int statics = in.readUnsignedShort();
                    used[0] += 2;
                    for (int i = 0; i < statics; i++) {
                        id(in, ids);
                        int t = in.readUnsignedByte();
                        int s = typeSize(t, ids);
                        skip(in, s);
                        used[0] += ids + 1 + s;
                    }
                    int fields = in.readUnsignedShort();
                    used[0] += 2;
                    List<long[]> fl = new ArrayList<>();
                    for (int i = 0; i < fields; i++) {
                        long name = id(in, ids);
                        int t = in.readUnsignedByte();
                        fl.add(new long[] {name, t});
                        used[0] += ids + 1;
                    }
                    if (cid == f.markerClass) f.markerFields = fl;
                }
                case 0x21 -> {
                    long oid = id(in, ids);
                    in.readInt();
                    long cid = id(in, ids);
                    int n = in.readInt();
                    byte[] b = new byte[n];
                    in.readFully(b);
                    used[0] += 2L * ids + 8 + n;
                    if (cid == f.markerClass) {
                        f.markerIds.put(oid, (long) f.markerInstances.size());
                        f.markerInstances.add(b);
                    }
                }
                case 0x22 -> {
                    id(in, ids);
                    in.readInt();
                    int n = in.readInt();
                    id(in, ids);
                    skip(in, (long) n * ids);
                    used[0] += 2L * ids + 8 + (long) n * ids;
                }
                case 0x23 -> {
                    long oid = id(in, ids);
                    in.readInt();
                    int n = in.readInt();
                    int t = in.readUnsignedByte();
                    long bytes = (long) n * typeSize(t, ids);
                    if (t == 8 && n >= 16 && n < 64) {
                        byte[] b = new byte[n];
                        in.readFully(b);
                        f.byteArrays.put(oid, Arrays.hashCode(b));
                    } else {
                        skip(in, bytes);
                    }
                    used[0] += ids + 9 + bytes;
                }
                default -> throw new IllegalStateException("hprof heap sub-record 0x" + Integer.toHexString(sub));
            }
        }
        Fz.expect(used[0] == end, "heap segment length");
    }

    /** Decodes every Marker instance and returns {count, sum v, sum w, sum ref hash, chain links}. */
    static long[] markers(Found f) {
        Fz.expect(f.markerClass != -1 && f.markerFields.size() == 4, "Marker class dump");
        long count = 0, sv = 0, sw = 0, sref = 0, links = 0, missing = 0;
        for (byte[] inst : f.markerInstances) {
            ByteBuffer b = ByteBuffer.wrap(inst);
            for (long[] field : f.markerFields) {
                String name = f.strings.get(field[0]);
                int t = (int) field[1];
                long value = t == 10 ? b.getInt() : t == 11 ? b.getLong() : f.idSize == 8 ? b.getLong() : b.getInt() & 0xffffffffL;
                switch (name) {
                    case "v" -> sv += value;
                    case "w" -> sw += value;
                    case "ref" -> {
                        Integer h = f.byteArrays.get(value);
                        if (h == null) missing++;
                        else sref += h;
                    }
                    case "next" -> { if (value != 0 && f.markerIds.containsKey(value)) links++; }
                    default -> throw new IllegalStateException("field " + name);
                }
            }
            count++;
        }
        return new long[] {count, sv, sw, sref, links, missing};
    }

    static void heapDumps() throws Exception {
        int n = 300 + Fz.rnd.nextInt(200) * Fz.scale;
        keep = new Marker[n];
        long sv = 0, sw = 0, sref = 0;
        Marker prev = null;
        for (int i = 0; i < n; i++) {
            byte[] b = Fz.bytes(16 + Fz.rnd.nextInt(48));
            int v = Fz.rnd.nextInt();
            long w = Fz.rnd.nextLong();
            keep[i] = prev = new Marker(v, w, b, i % 3 == 0 ? null : prev);
            sv += v;
            sw += w;
            sref += Arrays.hashCode(b);
        }
        long links = Arrays.stream(keep).filter(m -> m.next != null).count();
        HotSpotDiagnosticMXBean mx = ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        Path dir = Files.createTempDirectory("fzheap");
        long[] want = {n, sv, sw, sref, links, 0};
        for (boolean live : new boolean[] {true, false}) {
            Path path = dir.resolve("heap-" + live + ".hprof");
            mx.dumpHeap(path.toString(), live);
            Found f = parse(path);
            long[] got = markers(f);
            Fz.out("dump live=" + live, "markers=" + got[0] + " match=" + Arrays.equals(got, want)
                + " idSize=" + f.idSize + " threads>0=" + (f.threadRoots > 0) + " segments>0=" + (f.heapSegments > 0));
            Fz.expect(Arrays.equals(got, want), "heap dump contents live=" + live + " " + Arrays.toString(got) + " vs " + Arrays.toString(want));
            Files.delete(path);
        }
        Files.delete(dir);
        Fz.mix(sv);
        Fz.mix(sref);
    }

    static String nmt(MBeanServer mb, String... args) throws Exception {
        return (String) mb.invoke(new ObjectName("com.sun.management:type=DiagnosticCommand"), "vmNativeMemory",
            new Object[] {args}, new String[] {String[].class.getName()});
    }

    /** The "+NKB" change on the first line naming `category` in a summary diff, or -1. */
    static long growth(String diff, String category) {
        for (String line : diff.lines().toList()) {
            int at = line.indexOf(category + " (reserved=");
            if (at < 0) continue;
            String rest = line.substring(at);
            int plus = rest.indexOf('+'), kb = plus < 0 ? -1 : rest.indexOf("KB", plus);
            if (plus < 0 || kb < 0 || rest.indexOf(',') < plus) return 0;
            return Long.parseLong(rest.substring(plus + 1, kb));
        }
        return -1;
    }

    static void nativeMemory() throws Exception {
        MBeanServer mb = ManagementFactory.getPlatformMBeanServer();
        String summary = nmt(mb, "summary");
        boolean enabled = !summary.contains("not enabled");
        Fz.out("nmtEnabled", enabled);
        Fz.expect(enabled, "run with -XX:NativeMemoryTracking=detail");
        Fz.out("nmtSummary", summary.contains("Native Memory Tracking") + " heap=" + summary.contains("Java Heap")
            + " threads=" + summary.contains("Thread (reserved="));
        String detail = nmt(mb, "detail");
        Fz.out("nmtDetail", detail.contains("Virtual memory map") + " sites=" + detail.contains("[0x")
            + " heapRegion=" + detail.contains("for Java Heap"));
        Fz.out("nmtBaseline", nmt(mb, "baseline").contains("Baseline taken"));
        int mb8 = 8 + Fz.rnd.nextInt(8);
        ByteBuffer direct = ByteBuffer.allocateDirect(mb8 << 20);
        direct.put(0, (byte) 1);
        String diff = nmt(mb, "summary.diff");
        long other = growth(diff, "Other");
        Fz.out("nmtDiff", "otherGrewBy>=" + mb8 + "MB=" + (other >= (mb8 << 10)));
        String detailDiff = nmt(mb, "detail.diff");
        Fz.out("nmtDetailDiff", detailDiff.contains("Native Memory Tracking"));
        Fz.out("nmtDirectHeld", direct.get(0));
        Fz.mix(mb8);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("HeapNmt", args);
        heapDumps();
        nativeMemory();
        Fz.done();
    }
}
