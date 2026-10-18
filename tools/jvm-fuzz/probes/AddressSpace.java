import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.SecureRandom;
import java.util.List;
import java.util.Random;

/**
 * Values that differ between native runs but must be identical in a simulation and its
 * replay: the memory layout, the kernel's AT_RANDOM bytes, identity hashes and entropy-seeded
 * random numbers. Compared with replay only.
 */
public final class AddressSpace {
    public static void main(String[] args) throws Exception {
        Fz.init("AddressSpace", args);
        List<String> maps = Files.readAllLines(Path.of("/proc/self/maps"));
        long layout = 17;
        String libjvm = "-", stack = "-", heap = "-";
        for (String line : maps) {
            String range = line.substring(0, line.indexOf(' '));
            layout = layout * 31 + range.hashCode();
            if (line.endsWith("libjvm.so") && libjvm.equals("-")) libjvm = range;
            if (line.endsWith("[stack]")) stack = range;
            if (line.endsWith("[heap]")) heap = range;
        }
        Fz.out("regions", maps.size());
        Fz.outHex("layout", layout);
        Fz.out("libjvm", libjvm);
        Fz.out("stack", stack);
        Fz.out("heap", heap);
        ByteBuffer auxv = ByteBuffer.wrap(Files.readAllBytes(Path.of("/proc/self/auxv"))).order(ByteOrder.LITTLE_ENDIAN);
        long atRandom = 0;
        while (auxv.remaining() >= 16) {
            long type = auxv.getLong(), value = auxv.getLong();
            if (type == 25) atRandom = value; /* AT_RANDOM: address of 16 random bytes */
        }
        Fz.outHex("atRandomAddress", atRandom);
        long ih = 0;
        for (int i = 0; i < 1000; i++) ih = ih * 31 + System.identityHashCode(new Object());
        Fz.outHex("identityHashes", ih);
        Fz.out("random", new Random().nextLong());
        Fz.out("secureRandom", new SecureRandom().nextLong());
        Fz.out("strongRandom", SecureRandom.getInstanceStrong().nextLong());
        Fz.out("nanoTime", System.nanoTime());
        Fz.done();
    }
}
