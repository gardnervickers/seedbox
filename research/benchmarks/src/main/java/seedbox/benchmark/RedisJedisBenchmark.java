package seedbox.benchmark;

import com.google.gson.Gson;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import redis.clients.jedis.Jedis;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.Map;

/** Single operation stream; guest Linux must supply controlled monotonic time.
 * stdin/stdout are reliable control transport; Redis TCP uses faultable links.
 * Normal/check witness modes belong to the world, not this application oracle.
 */
public final class RedisJedisBenchmark {
    private static final Gson JSON = new Gson();
    private final JsonObject profile;
    private final JsonObject work;
    private final long seed, duration, interval;
    private final int keys, payloadBytes, operations, writeEvery, timeout;
    private final String namespace;
    private final String[] expected;
    private final MessageDigest digest = MessageDigest.getInstance("SHA-256");

    private RedisJedisBenchmark(Path path) throws Exception {
        profile = JsonParser.parseString(Files.readString(path)).getAsJsonObject();
        work = profile.getAsJsonObject("workload");
        seed = profile.get("seed").getAsLong();
        duration = work.get("duration_ns").getAsLong();
        int rate = work.get("rate_per_second").getAsInt();
        operations = work.get("operations").getAsInt();
        keys = work.get("key_count").getAsInt();
        payloadBytes = work.get("payload_bytes").getAsInt();
        writeEvery = work.get("write_every_n").getAsInt();
        timeout = work.get("timeout_ms").getAsInt();
        if (rate <= 0 || 1_000_000_000L % rate != 0 || duration <= 0 ||
            keys <= 0 || payloadBytes < 64 || writeEvery < 2 || operations <= 0 ||
            work.get("concurrency").getAsInt() != 1 || timeout <= 0) {
            throw new IllegalArgumentException("invalid fixed single-stream profile");
        }
        interval = 1_000_000_000L / rate;
        if (Math.multiplyExact((long)operations, interval) != duration) {
            throw new IllegalArgumentException("operations*interval must equal duration");
        }
        namespace = "seedbox:" + profile.get("profile_id").getAsString() + ":";
        expected = new String[keys];
    }

    private static long mix64(long value) {
        value = (value ^ (value >>> 30)) * 0xbf58476d1ce4e5b9L;
        value = (value ^ (value >>> 27)) * 0x94d049bb133111ebL;
        return value ^ (value >>> 31);
    }

    private int keyFor(long operation) {
        return (int)Long.remainderUnsigned(mix64(seed ^ operation), keys);
    }

    private String valueFor(long operation, int key) {
        String header = "seed=" + Long.toUnsignedString(seed) + ";op=" + operation + ";key=" + key + ";";
        if (header.length() > payloadBytes) throw new IllegalArgumentException("payload too short");
        char[] value = new char[payloadBytes];
        Arrays.fill(value, (char)('a' + Long.remainderUnsigned(mix64(seed ^ operation ^ key), 26)));
        header.getChars(0, header.length(), value, 0);
        return new String(value);
    }

    private String key(int index) { return namespace + "key:" + index; }

    private static void emit(Map<String, Object> values) {
        System.out.println(JSON.toJson(values));
        System.out.flush();
    }

    private static void require(boolean condition, String message) {
        if (!condition) throw new IllegalStateException(message);
    }

    private static void sleepUntil(long target) throws InterruptedException {
        long remaining;
        while ((remaining = target - System.nanoTime()) > 0) {
            Thread.sleep(remaining / 1_000_000L, (int)(remaining % 1_000_000L));
        }
    }

    private void prepare(Jedis primary, Jedis replica) throws Exception {
        int warmup = work.get("warmup_operations").getAsInt();
        for (int i = 0; i < keys; i++) {
            expected[i] = valueFor(-1, i);
            require("OK".equals(primary.set(key(i), expected[i])), "initial SET not acknowledged");
        }
        MessageDigest warmDigest = MessageDigest.getInstance("SHA-256");
        for (int i = 0; i < warmup; i++) {
            int k = keyFor(i);
            if (i % writeEvery == 0) {
                String value = valueFor(-2L - i, k);
                require("OK".equals(primary.set(key(k), value)), "warmup SET not acknowledged");
                expected[k] = value;
            } else {
                require(expected[k].equals(primary.get(key(k))), "warmup read mismatch");
            }
            warmDigest.update(expected[k].getBytes(StandardCharsets.US_ASCII));
            // Warm timer and JIT paths as part of the excluded, pinned warmup.
            sleepUntil(System.nanoTime() + 1_000_000L);
        }
        for (int i = 0; i < keys; i++) {
            expected[i] = valueFor(-1, i);
            require("OK".equals(primary.set(key(i), expected[i])), "reconcile SET not acknowledged");
        }
        require(primary.waitReplicas(1, timeout * 5L) == 1, "replica not ready after warmup");
        for (int i = 0; i < keys; i++) {
            require(expected[i].equals(replica.get(key(i))), "replica warmup reconcile mismatch");
        }
        // This control read is the proposed whole-world post-warmup snapshot point.
        // Restarting a new JVM here is not equivalent to restoring that snapshot.
        emit(Map.of("phase", "ready", "profile_id", profile.get("profile_id").getAsString(),
                    "seed", seed, "next_operation", 0, "warmup_operations", warmup,
                    "jdk", System.getProperty("java.runtime.version"), "jedis", "5.2.0"));
    }

    private Map<String, Object> execute(Jedis primary, Jedis replica, long epoch, boolean corrupt) throws Exception {
        long maxLag = 0, readCount = 0, writeCount = 0, writesDuringPartition = 0;
        boolean rewritten = false;
        long rewriteAt = work.has("aof_rewrite_at_ns") ? work.get("aof_rewrite_at_ns").getAsLong() : 40_000_000_000L;
        emit(Map.of("phase", "started", "epoch_guest_monotonic_ns", epoch));
        for (int i = 0; i < operations; i++) {
            long target = Math.addExact(epoch, Math.multiplyExact((long)i, interval));
            sleepUntil(target);
            long elapsed = System.nanoTime() - epoch;
            if (!rewritten && rewriteAt >= 0 && elapsed >= rewriteAt) {
                require(primary.bgrewriteaof().contains("started"), "AOF rewrite not started");
                rewritten = true;
            }
            maxLag = Math.max(maxLag, System.nanoTime() - target);
            int k = keyFor(i);
            boolean write = i % writeEvery == 0;
            String actual;
            if (write) {
                actual = valueFor(i, k);
                require("OK".equals(primary.set(key(k), actual)), "measured SET not acknowledged: " + i);
                expected[k] = actual;
                writeCount++;
                long acknowledgedAt = System.nanoTime() - epoch;
                long partitionAt = profile.getAsJsonArray("faults").get(0).getAsJsonObject().get("at_ns").getAsLong();
                long healAt = profile.getAsJsonArray("faults").get(1).getAsJsonObject().get("at_ns").getAsLong();
                if (acknowledgedAt >= partitionAt && acknowledgedAt < healAt) writesDuringPartition++;
            } else {
                actual = primary.get(key(k));
                String oracle = expected[k];
                if (corrupt && i == 1) oracle = "deliberately wrong read oracle";
                require(oracle.equals(actual), "acknowledged-value read mismatch at operation " + i);
                readCount++;
            }
            digest.update((i + ":" + (write ? "SET" : "GET") + ":" + k + ":").getBytes(StandardCharsets.US_ASCII));
            digest.update(actual.getBytes(StandardCharsets.US_ASCII));
            digest.update((byte)'\n');
        }
        sleepUntil(epoch + duration);
        long activeEnd = System.nanoTime();
        require(activeEnd - epoch <= duration + 1_000_000_000L, "offered workload exceeded fixed active deadline");
        long drainLimit = work.get("recovery_drain_limit_ns").getAsLong();
        long completionDeadline = Math.addExact(epoch + duration, drainLimit);
        String marker = valueFor(operations, 0);
        require("OK".equals(primary.set(namespace + "final-marker", marker)), "final marker SET not acknowledged");
        long waitMillis = Math.max(1L, (completionDeadline - System.nanoTime()) / 1_000_000L);
        require(primary.waitReplicas(1, waitMillis) == 1, "replica catch-up WAIT failed");
        for (int k = 0; k < keys; k++) {
            require(expected[k].equals(replica.get(key(k))), "replica acknowledged-value mismatch at key " + k);
        }
        require(marker.equals(replica.get(namespace + "final-marker")), "replica final marker mismatch");
        if (rewritten) {
            while (primary.info("persistence").contains("aof_rewrite_in_progress:1")) {
                require(System.nanoTime() < completionDeadline, "AOF rewrite recovery deadline");
                sleepUntil(System.nanoTime() + 10_000_000L);
            }
            require(primary.info("persistence").contains("aof_last_bgrewrite_status:ok"), "AOF rewrite status failed");
        }
        long completed = System.nanoTime();
        require(completed <= completionDeadline, "post-horizon drain exceeded limit");
        Map<String, Object> canonical = new LinkedHashMap<>();
        canonical.put("profile_id", profile.get("profile_id").getAsString());
        canonical.put("seed", seed);
        canonical.put("operations", operations);
        canonical.put("reads", readCount);
        canonical.put("acknowledged_writes", writeCount);
        canonical.put("acknowledged_writes_during_partition_window", writesDuringPartition);
        canonical.put("replica_keys_verified", keys);
        canonical.put("aof_rewrite_completed", rewritten);
        canonical.put("outcome_sha256", java.util.HexFormat.of().formatHex(digest.digest()));
        return Map.of("phase", "complete", "status", "passed", "canonical", canonical,
                      "timing", Map.of("clock_domain", "guest_monotonic_in_simulator; native_monotonic_in_smoke",
                          "active_duration_ns", activeEnd - epoch,
                          "recovery_and_check_drain_ns", completed - activeEnd,
                          "post_horizon_drain_ns", completed - epoch - duration,
                          "end_to_end_duration_ns", completed - epoch,
                          "max_offer_lag_ns", maxLag));
    }

    public static void main(String[] args) {
        try {
            Map<String, String> options = new LinkedHashMap<>();
            for (int i = 0; i < args.length; i += 2) {
                if (i + 1 == args.length || options.put(args[i], args[i + 1]) != null)
                    throw new IllegalArgumentException("expected unique option/value pairs");
            }
            for (String option : options.keySet()) {
                require(java.util.Set.of("--profile", "--primary", "--replica", "--primary-port", "--replica-port", "--barrier", "--negative-control").contains(option), "unknown option " + option);
            }
            require(options.containsKey("--profile") && options.containsKey("--primary") && options.containsKey("--replica"), "profile and isolated endpoint hosts required");
            RedisJedisBenchmark benchmark = new RedisJedisBenchmark(Path.of(options.get("--profile")));
            try (Jedis primary = new Jedis(options.get("--primary"), Integer.parseInt(options.getOrDefault("--primary-port", "6379")), benchmark.timeout);
                 Jedis replica = new Jedis(options.get("--replica"), Integer.parseInt(options.getOrDefault("--replica-port", "6379")), benchmark.timeout)) {
                benchmark.prepare(primary, replica);
                String barrier = options.getOrDefault("--barrier", "stdin");
                require(barrier.equals("stdin") || barrier.equals("none"), "unknown barrier mode");
                long epoch = 0;
                if (barrier.equals("stdin")) {
                    String command = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8)).readLine();
                    require(command != null && command.matches("START [0-9]+"), "START epoch control command required");
                    epoch = Long.parseLong(command.substring(6));
                }
                if (epoch == 0) epoch = System.nanoTime();
                emit(benchmark.execute(primary, replica, epoch, Boolean.parseBoolean(options.getOrDefault("--negative-control", "false"))));
            }
        } catch (Exception error) {
            emit(Map.of("phase", "failed", "status", "failed", "error_class", error.getClass().getName(), "message", String.valueOf(error.getMessage())));
            System.exit(1);
        }
    }
}
