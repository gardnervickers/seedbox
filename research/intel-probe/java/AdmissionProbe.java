import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.management.ManagementFactory;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.Random;
import java.util.concurrent.ThreadLocalRandom;

/** Off-host diagnostic; Random serialization here is NOT a VM snapshot test. */
public final class AdmissionProbe {
    private static byte[] serialize(Random random) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream output = new ObjectOutputStream(bytes)) {
            output.writeObject(random);
        }
        return bytes.toByteArray();
    }

    private static Random restore(byte[] bytes) throws Exception {
        try (ObjectInputStream input = new ObjectInputStream(new ByteArrayInputStream(bytes))) {
            return (Random) input.readObject();
        }
    }

    private static byte[] consume(Random random) {
        byte[] bytes = new byte[256];
        random.nextBytes(bytes);
        return bytes;
    }

    private static String hash(byte[] bytes) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
    }

    private static void selfCheck() throws Exception {
        Random warmed = new Random(7);
        consume(warmed);
        byte[] saved = serialize(warmed);
        // This property represents a caller's world seed: Java Random does not
        // consult it. Each continuation starts from exactly the same saved state.
        System.setProperty("seedbox.worldSeed", "42");
        byte[] inherited42 = consume(restore(saved));
        System.setProperty("seedbox.worldSeed", "43");
        byte[] inherited43 = consume(restore(saved));
        if (!Arrays.equals(inherited42, inherited43)) {
            throw new AssertionError("restored Random state unexpectedly follows external seed property");
        }
        byte[] explicit42 = consume(new Random(42));
        byte[] explicit43 = consume(new Random(43));
        if (Arrays.equals(explicit42, explicit43)) {
            throw new AssertionError("explicit workload seed is not sensitive");
        }
        if (!Arrays.equals(explicit42, consume(new Random(42)))) {
            throw new AssertionError("explicit workload stream is not reproducible");
        }
        System.out.printf(
                "{\"scope\":\"serialized_Random_object_only_not_VM_restore\","
                + "\"external_seed_does_not_reset_saved_generator\":true,"
                + "\"explicit_workload_seed_sensitive\":true,"
                + "\"inherited_sha256\":\"%s\",\"explicit42_sha256\":\"%s\","
                + "\"explicit43_sha256\":\"%s\"}%n",
                hash(inherited42), hash(explicit42), hash(explicit43));
    }

    private static void observe(long workloadSeed) throws Exception {
        Random workload = new Random(workloadSeed);
        long cpuBefore = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        long nanosBefore = System.nanoTime();
        long checksum = 0;
        byte[][] live = new byte[64][];
        for (int i = 0; i < 250_000; i++) {
            checksum = Long.rotateLeft(checksum, 7) ^ workload.nextLong();
            if ((i & 127) == 0) {
                byte[] block = new byte[8192];
                block[0] = (byte) checksum;
                live[(i >>> 7) & 63] = block;
            }
            // Frequent application clocks, deliberately independent of payload RNG.
            if ((i & 3) == 0 && System.nanoTime() < nanosBefore) {
                throw new AssertionError("monotonic clock moved backward");
            }
        }
        long nanosAfter = System.nanoTime();
        long cpuAfter = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        SecureRandom secure = new SecureRandom();
        byte[] secureBytes = new byte[32];
        secure.nextBytes(secureBytes);
        // Live data crosses the loop fence, preventing allocation-elimination.
        for (byte[] block : live) if (block != null) checksum ^= block[0];
        System.out.printf(
                "{\"scope\":\"native_runtime_observation_not_seeded_VM_acceptance\","
                + "\"workload_seed\":%d,\"payload_checksum\":%d,"
                + "\"nano_before\":%d,\"nano_after\":%d,\"wall_ms\":%d,"
                + "\"thread_cpu_before\":%d,\"thread_cpu_after\":%d,"
                + "\"secure_algorithm\":\"%s\",\"secure_provider\":\"%s\","
                + "\"secure_sample_sha256\":\"%s\",\"thread_local_random\":%d}%n",
                workloadSeed, checksum, nanosBefore, nanosAfter, System.currentTimeMillis(),
                cpuBefore, cpuAfter, secure.getAlgorithm(), secure.getProvider().getName(),
                hash(secureBytes), ThreadLocalRandom.current().nextLong());
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("--self-check")) {
            selfCheck();
        } else if (args.length == 2 && args[0].equals("--observe")) {
            observe(Long.parseLong(args[1]));
        } else {
            throw new IllegalArgumentException("--self-check or --observe workloadSeed");
        }
    }
}
