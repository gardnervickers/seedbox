// Jepsen's Kafka queue workload (jepsen.redpanda.workload.queue, as used in the
// Bufstream 0.1.0 analysis) as a small Java client for Seedbox.
//
// Each worker thread is one Jepsen process: a transactional producer, a
// read_committed consumer and their transactional ID. A worker runs
// transactions of 1..4 micro-operations, each a send of a unique value to a
// random key or a 100 ms poll, and calls sendOffsetsToTransaction for its polls,
// exactly as Jepsen's `with-txn` does: intentional aborts with probability
// --abort-p at every step, abortTransaction after a body error or a
// non-timeout commit error, ProducerFenced/OutOfOrderSequence/Authorization
// errors are not aborted, timeouts are indeterminate. A worker opens a new
// producer, consumer and transactional ID after an indeterminate result or a
// failed abort, as Jepsen's non-reusable client does.
//
// Every invocation and completion is sent as one JSON line to the oracle
// (oracle:7100), which owns the history and the checker. Just before each
// EndTxn (commit or abort) the worker prints "@@END <commit|abort> ..." to the
// console, so the oracle can arm a Seedbox console trigger on it.
//
// --mode produce is the acks=all durability workload: one non-transactional
// idempotent producer sends batches of 20 unique values every 50 ms and reports
// each acknowledgement.
// --mode queue is Jepsen's queue workload: the produce workload plus
// --consumers consumers in one group (--group-protocol classic or consumer,
// KIP-848) that subscribe, poll and commitSync the offsets after each poll,
// reporting every assignment change, poll and commit. Once stopped, the
// consumers drain until the group's committed offsets reach every partition's
// end, and the client reports the group's final committed offsets.
// Every mode ends with a final read of every partition from offset 0.
import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

import org.apache.kafka.clients.admin.*;
import org.apache.kafka.clients.consumer.*;
import org.apache.kafka.clients.producer.*;
import org.apache.kafka.common.*;
import org.apache.kafka.common.errors.*;
import org.apache.kafka.common.serialization.*;

public final class JepsenQueue {
    static String name = "c1", bootstrap = "n1:9092", oracleHost = "oracle", mode = "txn", topic = "jq";
    static int oraclePort = 7100, workers = 2, keys = 8, rf = 1, maxTxnLength = 4, endDelayMs = 50;
    static int txnTimeoutMs = 1000, produceBatch = 20, producePauseMs = 50, consumers = 2, drainSeconds = 180;
    static String groupProtocol = "classic", group = "jepsen-queue";
    static volatile boolean drained = false;
    static double abortP = 1.0 / 32;
    static volatile boolean stop = false, started = false;
    static final AtomicLong nextValue = new AtomicLong(0), nextOp = new AtomicLong(0);
    static final AtomicInteger nextProcess = new AtomicInteger(0);
    static PrintStream sink;
    static final Random seedSource = new Random();

    // ---------------------------------------------------------------- JSON output

    static String q(String s) {
        StringBuilder b = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c == '"' || c == '\\') b.append('\\').append(c);
            else if (c < 0x20) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.append('"').toString();
    }

    static String json(Object o) {
        if (o == null) return "null";
        if (o instanceof String) return q((String) o);
        if (o instanceof Number || o instanceof Boolean) return o.toString();
        if (o instanceof Map) {
            StringBuilder b = new StringBuilder("{");
            for (Map.Entry<?, ?> e : ((Map<?, ?>) o).entrySet()) {
                if (b.length() > 1) b.append(',');
                b.append(q(String.valueOf(e.getKey()))).append(':').append(json(e.getValue()));
            }
            return b.append('}').toString();
        }
        if (o instanceof Collection) {
            StringBuilder b = new StringBuilder("[");
            for (Object x : (Collection<?>) o) {
                if (b.length() > 1) b.append(',');
                b.append(json(x));
            }
            return b.append(']').toString();
        }
        return q(o.toString());
    }

    static Map<String, Object> obj(Object... kv) {
        Map<String, Object> m = new LinkedHashMap<>();
        for (int i = 0; i < kv.length; i += 2) m.put((String) kv[i], kv[i + 1]);
        return m;
    }

    static synchronized void emit(Map<String, Object> event) {
        event.put("ms", System.nanoTime() / 1_000_000);
        sink.print(json(event) + "\n");
        sink.flush();
    }

    static void console(String line) {
        synchronized (System.out) {
            System.out.println(line);
            System.out.flush();
        }
    }

    static String err(Throwable t) {
        return t == null ? null : t.getClass().getSimpleName() + ": " + t.getMessage();
    }

    // ---------------------------------------------------------------- Kafka configuration (Jepsen's)

    static Properties producerConfig(String txid) {
        Properties p = new Properties();
        p.put(ProducerConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrap);
        p.put(ProducerConfig.KEY_SERIALIZER_CLASS_CONFIG, LongSerializer.class.getName());
        p.put(ProducerConfig.VALUE_SERIALIZER_CLASS_CONFIG, LongSerializer.class.getName());
        p.put(ProducerConfig.DELIVERY_TIMEOUT_MS_CONFIG, 10000);
        p.put(ProducerConfig.REQUEST_TIMEOUT_MS_CONFIG, 3000);
        p.put(ProducerConfig.MAX_BLOCK_MS_CONFIG, 10000);
        p.put(ProducerConfig.RECONNECT_BACKOFF_MAX_MS_CONFIG, 1000);
        p.put(ProducerConfig.SOCKET_CONNECTION_SETUP_TIMEOUT_MS_CONFIG, 500);
        p.put(ProducerConfig.SOCKET_CONNECTION_SETUP_TIMEOUT_MAX_MS_CONFIG, 1000);
        p.put(ProducerConfig.ACKS_CONFIG, "all");
        p.put(ProducerConfig.ENABLE_IDEMPOTENCE_CONFIG, true);
        p.put(ProducerConfig.RETRIES_CONFIG, 1000);
        if (txid != null) {
            p.put(ProducerConfig.TRANSACTION_TIMEOUT_CONFIG, txnTimeoutMs);
            p.put(ProducerConfig.TRANSACTIONAL_ID_CONFIG, txid);
            p.put(ProducerConfig.CLIENT_ID_CONFIG, "jepsen-" + txid);
        }
        return p;
    }

    static Properties consumerConfig(boolean group) {
        Properties p = new Properties();
        p.put(ConsumerConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrap);
        p.put(ConsumerConfig.KEY_DESERIALIZER_CLASS_CONFIG, LongDeserializer.class.getName());
        p.put(ConsumerConfig.VALUE_DESERIALIZER_CLASS_CONFIG, LongDeserializer.class.getName());
        p.put(ConsumerConfig.SOCKET_CONNECTION_SETUP_TIMEOUT_MAX_MS_CONFIG, 1000);
        p.put(ConsumerConfig.SOCKET_CONNECTION_SETUP_TIMEOUT_MS_CONFIG, 500);
        p.put(ConsumerConfig.METADATA_MAX_AGE_CONFIG, 60000);
        p.put(ConsumerConfig.REQUEST_TIMEOUT_MS_CONFIG, 10000);
        p.put(ConsumerConfig.DEFAULT_API_TIMEOUT_MS_CONFIG, 10000);
        p.put(ConsumerConfig.HEARTBEAT_INTERVAL_MS_CONFIG, 300);
        p.put(ConsumerConfig.SESSION_TIMEOUT_MS_CONFIG, 6000);
        p.put(ConsumerConfig.CONNECTIONS_MAX_IDLE_MS_CONFIG, 60000);
        // sendOffsetsToTransaction needs a group ID even with assign().
        if (group) p.put(ConsumerConfig.GROUP_ID_CONFIG, "jepsen-group");
        p.put(ConsumerConfig.ISOLATION_LEVEL_CONFIG, "read_committed");
        p.put(ConsumerConfig.AUTO_OFFSET_RESET_CONFIG, "earliest");
        p.put(ConsumerConfig.ENABLE_AUTO_COMMIT_CONFIG, false);
        return p;
    }

    static List<TopicPartition> partitions() {
        List<TopicPartition> tps = new ArrayList<>();
        for (int k = 0; k < keys; k++) tps.add(new TopicPartition(topic, k));
        return tps;
    }

    // ---------------------------------------------------------------- one Jepsen process

    static final class IntentionalAbort extends RuntimeException {
        IntentionalAbort() { super("intentional-abort"); }
    }

    /** Jepsen's {:type :abort ...} wrapper. */
    static final class AbortResult extends RuntimeException {
        final boolean abortOk, triedCommit, definite;
        final Throwable body, abortError;
        AbortResult(boolean abortOk, boolean triedCommit, boolean definite, Throwable body, Throwable abortError) {
            super("abort");
            this.abortOk = abortOk; this.triedCommit = triedCommit; this.definite = definite;
            this.body = body; this.abortError = abortError;
        }
    }

    static final class Worker implements Runnable {
        final int w;
        final Random rng;
        int process;
        String txid;
        KafkaProducer<Long, Long> producer;
        KafkaConsumer<Long, Long> consumer;

        Worker(int w) { this.w = w; this.rng = new Random(seedSource.nextLong()); }

        void open() {
            while (!stop) {
                process = nextProcess.getAndIncrement();
                txid = "jt" + process;
                emit(obj("type", "open", "w", w, "process", process, "txid", txid));
                try {
                    producer = new KafkaProducer<>(producerConfig(txid));
                    producer.initTransactions();
                    consumer = new KafkaConsumer<>(consumerConfig(true));
                    consumer.assign(partitions());
                    consumer.seekToBeginning(partitions());
                    return;
                } catch (RuntimeException e) {
                    emit(obj("type", "open-failed", "w", w, "process", process, "error", err(e)));
                    close();
                    sleep(500);
                }
            }
        }

        void close() {
            try { if (producer != null) producer.close(Duration.ZERO); } catch (RuntimeException ignored) { }
            try { if (consumer != null) consumer.close(Duration.ZERO); } catch (RuntimeException ignored) { }
            producer = null; consumer = null;
        }

        void maybeAbort() {
            if (rng.nextDouble() < abortP) throw new IntentionalAbort();
        }

        List<Object> generate() {
            int n = 1 + rng.nextInt(maxTxnLength);
            List<Object> mops = new ArrayList<>();
            for (int i = 0; i < n; i++) {
                if (rng.nextBoolean()) {
                    long k = rng.nextInt(keys);
                    mops.add(new ArrayList<>(List.of("send", k, nextValue.getAndIncrement())));
                } else {
                    mops.add(new ArrayList<>(List.of("poll")));
                }
            }
            return mops;
        }

        @SuppressWarnings("unchecked")
        Object mop(List<Object> m) {
            if (m.get(0).equals("poll")) {
                ConsumerRecords<Long, Long> records = consumer.poll(Duration.ofMillis(100));
                Map<String, Object> byKey = new TreeMap<>();
                for (TopicPartition tp : records.partitions()) {
                    List<Object> pairs = new ArrayList<>();
                    for (ConsumerRecord<Long, Long> r : records.records(tp)) pairs.add(List.of(r.offset(), r.value()));
                    byKey.put(String.valueOf(tp.partition()), pairs);
                }
                return new ArrayList<>(List.of("poll", byKey));
            }
            long k = (Long) m.get(1), v = (Long) m.get(2);
            RecordMetadata md;
            try {
                md = producer.send(new ProducerRecord<>(topic, (int) k, null, v)).get(10, TimeUnit.SECONDS);
            } catch (ExecutionException e) {
                Throwable c = e.getCause();
                if (c instanceof RuntimeException) throw (RuntimeException) c;
                throw new KafkaException(c);
            } catch (java.util.concurrent.TimeoutException e) {
                throw new LocalTimeout();
            } catch (InterruptedException e) {
                throw new InterruptException(e);
            }
            if (md.partition() != k) throw new IllegalStateException("key mismatch " + k + " " + md.partition());
            return new ArrayList<>(List.of("send", k, Arrays.asList(md.hasOffset() ? md.offset() : null, v)));
        }

        @SuppressWarnings("unchecked")
        void sendOffsets(List<Object> value) {
            Map<TopicPartition, OffsetAndMetadata> offsets = new HashMap<>();
            for (Object o : value) {
                List<Object> m = (List<Object>) o;
                if (!m.get(0).equals("poll") || m.size() < 2) continue;
                for (Map.Entry<String, Object> e : ((Map<String, Object>) m.get(1)).entrySet()) {
                    TopicPartition tp = new TopicPartition(topic, Integer.parseInt(e.getKey()));
                    for (Object pair : (List<Object>) e.getValue()) {
                        long off = (Long) ((List<Object>) pair).get(0);
                        OffsetAndMetadata prev = offsets.get(tp);
                        if (prev == null || prev.offset() < off + 1) offsets.put(tp, new OffsetAndMetadata(off + 1));
                    }
                }
            }
            if (!offsets.isEmpty()) producer.sendOffsetsToTransaction(offsets, consumer.groupMetadata());
        }

        @SuppressWarnings("unchecked")
        void rollback(List<Object> value) {
            Map<Integer, Long> first = new HashMap<>();
            for (Object o : value) {
                List<Object> m = (List<Object>) o;
                if (!m.get(0).equals("poll") || m.size() < 2) continue;
                for (Map.Entry<String, Object> e : ((Map<String, Object>) m.get(1)).entrySet()) {
                    List<Object> pairs = (List<Object>) e.getValue();
                    if (!pairs.isEmpty()) first.putIfAbsent(Integer.parseInt(e.getKey()), (Long) ((List<Object>) pairs.get(0)).get(0));
                }
            }
            for (Map.Entry<Integer, Long> e : first.entrySet()) {
                try { consumer.seek(new TopicPartition(topic, e.getKey()), e.getValue()); } catch (RuntimeException ignored) { }
            }
        }

        void safeAbort(long op, boolean triedCommit, RuntimeException body) {
            console("@@END abort " + name + " w" + w + " op" + op + " " + txid);
            emit(obj("type", "end", "w", w, "op", op, "kind", "abort", "txid", txid));
            sleep(endDelayMs);
            try {
                producer.abortTransaction();
            } catch (RuntimeException abortError) {
                throw new AbortResult(false, triedCommit,
                        !triedCommit || body instanceof InvalidTxnStateException, body, abortError);
            }
            throw new AbortResult(true, triedCommit, true, body, null);
        }

        @SuppressWarnings("unchecked")
        void txn(long op, List<Object> value) {
            producer.beginTransaction();
            try {
                for (int i = 0; i < value.size(); i++) {
                    maybeAbort();
                    value.set(i, mop((List<Object>) value.get(i)));
                }
                maybeAbort();
                sendOffsets(value);
            } catch (ProducerFencedException | OutOfOrderSequenceException | AuthorizationException e) {
                throw e;
            } catch (AbortResult e) {
                throw e;
            } catch (RuntimeException e) {
                safeAbort(op, false, e);
            }
            console("@@END commit " + name + " w" + w + " op" + op + " " + txid);
            emit(obj("type", "end", "w", w, "op", op, "kind", "commit", "txid", txid));
            sleep(endDelayMs);
            try {
                producer.commitTransaction();
            } catch (ProducerFencedException | OutOfOrderSequenceException | AuthorizationException e) {
                throw e;
            } catch (org.apache.kafka.common.errors.TimeoutException | InterruptException e) {
                throw e;
            } catch (RuntimeException e) {
                safeAbort(op, true, e);
            }
        }

        /** Jepsen's with-errors: returns {type, error, end-process}. */
        Object[] classify(Throwable e) {
            if (e instanceof AbortResult) {
                AbortResult a = (AbortResult) e;
                return new Object[]{a.definite ? "fail" : "info",
                        obj("abort-ok", a.abortOk, "tried-commit", a.triedCommit, "definite", a.definite,
                                "body-error", err(a.body), "abort-error", err(a.abortError)), !a.abortOk};
            }
            String m = String.valueOf(e.getMessage());
            if (e instanceof AuthorizationException) return new Object[]{"fail", "authorization", true};
            if (e instanceof DisconnectException) return new Object[]{"info", err(e), false};
            if (e instanceof IllegalStateException) return new Object[]{"info", err(e), true};
            if (e instanceof InvalidProducerEpochException) return new Object[]{"fail", err(e), false};
            if (e instanceof NetworkException) return new Object[]{"info", err(e), false};
            if (e instanceof NotLeaderOrFollowerException) return new Object[]{"info", err(e), false};
            if (e instanceof OutOfOrderSequenceException) return new Object[]{"fail", err(e), true};
            if (e instanceof ProducerFencedException) return new Object[]{"fail", err(e), true};
            if (e instanceof UnknownTopicOrPartitionException) return new Object[]{"fail", err(e), false};
            if (e instanceof UnknownServerException) return new Object[]{"info", err(e), false};
            if (e instanceof org.apache.kafka.common.errors.TimeoutException) return new Object[]{"info", err(e), false};
            if (e instanceof LocalTimeout) return new Object[]{"info", "timeout", false};
            if (e instanceof KafkaException) {
                if (m.contains("broker is not available")) return new Object[]{"fail", err(e), false};
                if (m.contains("Cannot execute transactional method because we are in an error state"))
                    return new Object[]{"fail", err(e), true};
                if (m.contains("Unhandled error in EndTxnResponse")) return new Object[]{"info", err(e), false};
            }
            // Anything else: indeterminate, and this process ends.
            return new Object[]{"info", err(e), true};
        }

        @SuppressWarnings("unchecked")
        public void run() {
            open();
            while (!stop) {
                long op = nextOp.getAndIncrement();
                List<Object> value = generate();
                emit(obj("type", "invoke", "w", w, "process", process, "op", op, "f", "txn", "value", value,
                        "txid", txid));
                String type;
                Object error = null;
                boolean end = false;
                try {
                    txn(op, value);
                    type = "ok";
                } catch (Throwable e) {
                    Object[] c = classify(e);
                    type = (String) c[0]; error = c[1]; end = (Boolean) c[2];
                }
                if (!type.equals("ok")) rollback(value);
                emit(obj("type", type, "w", w, "process", process, "op", op, "f", "txn", "value", value,
                        "error", error, "txid", txid));
                // Jepsen's client is not reusable: an indeterminate result or a failed abort crashes the process.
                if (type.equals("info") || end) {
                    close();
                    open();
                }
            }
            close();
        }
    }

    static final class LocalTimeout extends RuntimeException {
        LocalTimeout() { super("send timed out after 10 s"); }
    }

    static void sleep(long ms) {
        if (ms <= 0) return;
        try { Thread.sleep(ms); } catch (InterruptedException e) { Thread.currentThread().interrupt(); }
    }

    // ---------------------------------------------------------------- produce mode (acks=all durability)

    static void produce() {
        KafkaProducer<Long, Long> producer = new KafkaProducer<>(producerConfig(null));
        long seq = 0;
        while (!stop) {
            List<long[]> batch = new ArrayList<>();
            List<Future<RecordMetadata>> futures = new ArrayList<>();
            for (int i = 0; i < produceBatch; i++, seq++) {
                long k = seq % keys, v = nextValue.getAndIncrement();
                batch.add(new long[]{k, v});
                emit(obj("type", "invoke", "f", "send", "k", k, "v", v));
                futures.add(producer.send(new ProducerRecord<>(topic, (int) k, null, v)));
            }
            for (int i = 0; i < futures.size(); i++) {
                long k = batch.get(i)[0], v = batch.get(i)[1];
                try {
                    RecordMetadata md = futures.get(i).get(15, TimeUnit.SECONDS);
                    emit(obj("type", "ok", "f", "send", "k", k, "v", v, "offset", md.hasOffset() ? md.offset() : null));
                } catch (ExecutionException e) {
                    emit(obj("type", "info",
                            "f", "send", "k", k, "v", v, "error", err(e.getCause())));
                } catch (Exception e) {
                    emit(obj("type", "info", "f", "send", "k", k, "v", v, "error", err(e)));
                }
            }
            sleep(producePauseMs);
        }
        try { producer.close(Duration.ofSeconds(1)); } catch (RuntimeException ignored) { }
    }

    // ---------------------------------------------------------------- queue mode (consumer group)

    static Map<String, Object> byPartition(Map<TopicPartition, OffsetAndMetadata> offsets) {
        Map<String, Object> out = new TreeMap<>();
        for (Map.Entry<TopicPartition, OffsetAndMetadata> e : offsets.entrySet())
            out.put(String.valueOf(e.getKey().partition()), e.getValue() == null ? null : e.getValue().offset());
        return out;
    }

    static List<Object> partitionList(Collection<TopicPartition> tps) {
        List<Integer> ids = new ArrayList<>();
        for (TopicPartition tp : tps) ids.add(tp.partition());
        Collections.sort(ids);
        return new ArrayList<Object>(ids);
    }

    static Properties groupConfig() {
        Properties p = consumerConfig(false);
        p.put(ConsumerConfig.GROUP_ID_CONFIG, group);
        p.put("group.protocol", groupProtocol);
        if (groupProtocol.equals("consumer")) {
            // KIP-848 groups take their timeouts from the broker.
            p.remove(ConsumerConfig.HEARTBEAT_INTERVAL_MS_CONFIG);
            p.remove(ConsumerConfig.SESSION_TIMEOUT_MS_CONFIG);
        }
        return p;
    }

    /** One group member: poll, report, commitSync, until the group has drained. */
    static final class QueueConsumer implements Runnable {
        final int c;
        int incarnation = 0;

        QueueConsumer(int c) { this.c = c; }

        public void run() {
            while (!drained) {
                incarnation++;
                final int inc = incarnation;
                KafkaConsumer<Long, Long> consumer = null;
                try {
                    consumer = new KafkaConsumer<>(groupConfig());
                    emit(obj("type", "consumer-open", "c", c, "inc", inc, "protocol", groupProtocol));
                    consumer.subscribe(List.of(topic), new ConsumerRebalanceListener() {
                        public void onPartitionsRevoked(Collection<TopicPartition> tps) {
                            emit(obj("type", "revoked", "c", c, "inc", inc, "partitions", partitionList(tps)));
                        }
                        public void onPartitionsAssigned(Collection<TopicPartition> tps) {
                            emit(obj("type", "assigned", "c", c, "inc", inc, "partitions", partitionList(tps)));
                        }
                        public void onPartitionsLost(Collection<TopicPartition> tps) {
                            emit(obj("type", "lost", "c", c, "inc", inc, "partitions", partitionList(tps)));
                        }
                    });
                    while (!drained) {
                        ConsumerRecords<Long, Long> records = consumer.poll(Duration.ofMillis(200));
                        if (records.isEmpty()) continue;
                        Map<String, Object> byKey = new TreeMap<>();
                        Map<TopicPartition, OffsetAndMetadata> offsets = new HashMap<>();
                        for (TopicPartition tp : records.partitions()) {
                            List<Object> pairs = new ArrayList<>();
                            long last = -1;
                            for (ConsumerRecord<Long, Long> r : records.records(tp)) {
                                pairs.add(List.of(r.offset(), r.value()));
                                last = r.offset();
                            }
                            byKey.put(String.valueOf(tp.partition()), pairs);
                            offsets.put(tp, new OffsetAndMetadata(last + 1));
                        }
                        emit(obj("type", "poll", "c", c, "inc", inc, "records", byKey));
                        try {
                            consumer.commitSync(offsets, Duration.ofSeconds(10));
                            emit(obj("type", "commit", "result", "ok", "c", c, "inc", inc, "offsets", byPartition(offsets)));
                        } catch (CommitFailedException | RebalanceInProgressException | FencedInstanceIdException
                                 | UnknownMemberIdException | IllegalGenerationException e) {
                            emit(obj("type", "commit", "result", "fail", "c", c, "inc", inc, "offsets", byPartition(offsets), "error", err(e)));
                        } catch (WakeupException | InterruptException e) {
                            throw e;
                        } catch (RuntimeException e) {
                            emit(obj("type", "commit", "result", "info", "c", c, "inc", inc, "offsets", byPartition(offsets), "error", err(e)));
                        }
                    }
                } catch (RuntimeException e) {
                    emit(obj("type", "consumer-error", "c", c, "inc", inc, "error", err(e)));
                    sleep(500);
                } finally {
                    try { if (consumer != null) consumer.close(Duration.ofSeconds(5)); } catch (RuntimeException ignored) { }
                    emit(obj("type", "consumer-closed", "c", c, "inc", inc));
                }
            }
        }
    }

    static Map<TopicPartition, Long> committedOffsets(Admin admin) throws Exception {
        Map<TopicPartition, OffsetAndMetadata> m =
                admin.listConsumerGroupOffsets(group).partitionsToOffsetAndMetadata().get(10, TimeUnit.SECONDS);
        Map<TopicPartition, Long> out = new HashMap<>();
        for (Map.Entry<TopicPartition, OffsetAndMetadata> e : m.entrySet())
            if (e.getValue() != null) out.put(e.getKey(), e.getValue().offset());
        return out;
    }

    static Properties adminConfig() {
        Properties p = new Properties();
        p.put(AdminClientConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrap);
        p.put(AdminClientConfig.REQUEST_TIMEOUT_MS_CONFIG, 10000);
        p.put(AdminClientConfig.DEFAULT_API_TIMEOUT_MS_CONFIG, 20000);
        return p;
    }

    /**
     * After the producer stops: wait until the group's committed offset of every partition
     * equals its end offset (two equal observations a second apart), then stop the consumers.
     */
    static void drain() {
        long deadline = System.nanoTime() + drainSeconds * 1_000_000_000L;
        String status = "timeout";
        Map<TopicPartition, Long> committed = null, end = null;
        String previous = null;
        try (Admin admin = Admin.create(adminConfig());
             KafkaConsumer<Long, Long> probe = new KafkaConsumer<>(consumerConfig(false))) {
            while (System.nanoTime() < deadline) {
                try {
                    end = probe.endOffsets(partitions(), Duration.ofSeconds(10));
                    committed = committedOffsets(admin);
                    boolean done = true;
                    for (TopicPartition tp : partitions()) {
                        long e = end.get(tp);
                        if (e > 0 && !Long.valueOf(e).equals(committed.get(tp))) done = false;
                    }
                    String now = String.valueOf(offsets(end)) + offsets(committed);
                    if (done && now.equals(previous)) { status = "complete"; break; }
                    previous = done ? now : null;
                } catch (Exception e) {
                    emit(obj("type", "drain-retry", "error", err(e)));
                    previous = null;
                }
                sleep(1000);
            }
        }
        emit(obj("type", "drained", "status", status, "committed", offsets(committed), "end-offsets", offsets(end)));
        drained = true;
    }

    /** The group's committed offsets once its members have closed. */
    static void groupOffsets() {
        for (int attempt = 0; attempt < 10; attempt++) {
            try (Admin admin = Admin.create(adminConfig())) {
                emit(obj("type", "group-offsets", "committed", offsets(committedOffsets(admin))));
                return;
            } catch (Exception e) {
                emit(obj("type", "group-offsets-retry", "error", err(e)));
                sleep(1000);
            }
        }
    }

    // ---------------------------------------------------------------- setup and final read

    static void createTopic() {
        Properties p = new Properties();
        p.put(AdminClientConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrap);
        p.put(AdminClientConfig.REQUEST_TIMEOUT_MS_CONFIG, 10000);
        p.put(AdminClientConfig.DEFAULT_API_TIMEOUT_MS_CONFIG, 20000);
        try (Admin admin = Admin.create(p)) {
            while (true) {
                try {
                    NewTopic t = new NewTopic(topic, keys, (short) rf);
                    if (rf > 1) t.configs(Map.of("min.insync.replicas", String.valueOf(Math.min(2, rf))));
                    admin.createTopics(List.of(t)).all().get();
                    emit(obj("type", "topic-created", "topic", topic, "partitions", keys, "rf", rf));
                    break;
                } catch (ExecutionException e) {
                    if (e.getCause() instanceof TopicExistsException) break;
                    emit(obj("type", "setup-retry", "error", err(e.getCause())));
                } catch (Exception e) {
                    emit(obj("type", "setup-retry", "error", err(e)));
                }
                sleep(1000);
            }
            // Wait until every partition has a leader.
            while (true) {
                try {
                    TopicDescription d = admin.describeTopics(List.of(topic)).allTopicNames().get().get(topic);
                    boolean ok = d.partitions().size() == keys;
                    List<Object> leaders = new ArrayList<>();
                    for (TopicPartitionInfo i : d.partitions()) {
                        ok &= i.leader() != null && i.isr().size() >= Math.min(2, rf);
                        leaders.add(i.leader() == null ? null : i.leader().id());
                    }
                    if (ok) {
                        emit(obj("type", "topic-ready", "leaders", leaders));
                        return;
                    }
                } catch (Exception e) {
                    emit(obj("type", "setup-retry", "error", err(e)));
                }
                sleep(500);
            }
        }
    }

    /**
     * Read every partition from offset 0 at read_committed. The read is complete when the
     * consumer reached the last stable offset of every partition and that offset equals the
     * high watermark, so no transaction is still open: an open transaction holds the last
     * stable offset until it ends or the coordinator times it out (transaction.timeout.ms).
     */
    static void finalRead() {
        List<TopicPartition> tps = partitions();
        Map<String, List<Object>> logs = new TreeMap<>();
        for (TopicPartition tp : tps) logs.put(String.valueOf(tp.partition()), new ArrayList<>());
        long deadline = System.nanoTime() + (txnTimeoutMs + 120_000L) * 1_000_000L;
        String complete = "timeout";
        Map<TopicPartition, Long> end = null, hw = null;
        Properties uncommitted = consumerConfig(false);
        uncommitted.put(ConsumerConfig.ISOLATION_LEVEL_CONFIG, "read_uncommitted");
        while (System.nanoTime() < deadline) {
            try (KafkaConsumer<Long, Long> c = new KafkaConsumer<>(consumerConfig(false));
                 KafkaConsumer<Long, Long> u = new KafkaConsumer<>(uncommitted)) {
                c.assign(tps);
                c.seekToBeginning(tps);
                for (List<Object> l : logs.values()) l.clear();
                end = c.endOffsets(tps, Duration.ofSeconds(10));
                hw = u.endOffsets(tps, Duration.ofSeconds(10));
                if (!end.equals(hw)) {
                    complete = "open-transactions";
                    emit(obj("type", "final-retry", "reason", complete, "lso", offsets(end), "hw", offsets(hw)));
                    sleep(5000);
                    continue;
                }
                Map<TopicPartition, Long> positions = new HashMap<>();
                long attemptEnd = System.nanoTime() + 60_000_000_000L;
                while (System.nanoTime() < attemptEnd) {
                    for (ConsumerRecord<Long, Long> r : c.poll(Duration.ofMillis(500)))
                        logs.get(String.valueOf(r.partition())).add(List.of(r.offset(), r.value()));
                    boolean done = true;
                    for (TopicPartition tp : tps) {
                        long pos = c.position(tp, Duration.ofSeconds(10));
                        positions.put(tp, pos);
                        if (pos < end.get(tp)) done = false;
                    }
                    if (done) break;
                }
                boolean done = true;
                for (TopicPartition tp : tps) if (positions.getOrDefault(tp, -1L) < end.get(tp)) done = false;
                // Neither offset may have moved meanwhile: a later read would see more.
                Map<TopicPartition, Long> again = c.endOffsets(tps, Duration.ofSeconds(10));
                Map<TopicPartition, Long> hwAgain = u.endOffsets(tps, Duration.ofSeconds(10));
                if (done && again.equals(end) && hwAgain.equals(hw)) { complete = "complete"; break; }
                complete = done ? "end-moved" : "incomplete";
                emit(obj("type", "final-retry", "reason", complete));
            } catch (RuntimeException e) {
                emit(obj("type", "final-retry", "error", err(e)));
                sleep(1000);
            }
        }
        emit(obj("type", "final", "status", complete, "logs", logs, "end-offsets", offsets(end), "hw", offsets(hw)));
    }

    static Map<String, Object> offsets(Map<TopicPartition, Long> m) {
        Map<String, Object> out = new TreeMap<>();
        if (m != null) for (Map.Entry<TopicPartition, Long> e : m.entrySet()) out.put(String.valueOf(e.getKey().partition()), e.getValue());
        return out;
    }

    // ---------------------------------------------------------------- main

    public static void main(String[] args) throws Exception {
        for (int i = 0; i + 1 < args.length; i += 2) {
            String k = args[i], v = args[i + 1];
            switch (k) {
                case "--name": name = v; break;
                case "--bootstrap": bootstrap = v; break;
                case "--oracle": oracleHost = v; break;
                case "--mode": mode = v; break;
                case "--workers": workers = Integer.parseInt(v); break;
                case "--keys": keys = Integer.parseInt(v); break;
                case "--rf": rf = Integer.parseInt(v); break;
                case "--abort-p": abortP = Double.parseDouble(v); break;
                case "--txn-timeout-ms": txnTimeoutMs = Integer.parseInt(v); break;
                case "--end-delay-ms": endDelayMs = Integer.parseInt(v); break;
                case "--max-txn-length": maxTxnLength = Integer.parseInt(v); break;
                case "--produce-pause-ms": producePauseMs = Integer.parseInt(v); break;
                case "--consumers": consumers = Integer.parseInt(v); break;
                case "--group-protocol": groupProtocol = v; break;
                case "--drain-seconds": drainSeconds = Integer.parseInt(v); break;
                default: throw new IllegalArgumentException("unknown argument " + k);
            }
        }
        Socket socket = null;
        while (socket == null) {
            try {
                socket = new Socket(oracleHost, oraclePort);
            } catch (IOException e) {
                sleep(200);
            }
        }
        socket.setTcpNoDelay(true);
        sink = new PrintStream(new BufferedOutputStream(socket.getOutputStream()), false, StandardCharsets.UTF_8);
        BufferedReader commands = new BufferedReader(new InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8));
        emit(obj("type", "hello", "client", name, "mode", mode, "workers", workers, "keys", keys,
                "consumers", consumers, "group-protocol", groupProtocol,
                "kafka-clients", AppInfoVersion.get(), "abort-p", abortP, "txn-timeout-ms", txnTimeoutMs));
        Thread reader = new Thread(() -> {
            try {
                String line;
                while ((line = commands.readLine()) != null) {
                    if (line.equals("start")) started = true;
                    if (line.equals("stop")) stop = true;
                }
            } catch (IOException ignored) { }
            stop = true;
        }, "oracle-commands");
        reader.setDaemon(true);
        reader.start();
        createTopic();
        while (!started && !stop) sleep(50);
        emit(obj("type", "started"));
        if (mode.equals("produce")) {
            produce();
        } else if (mode.equals("queue")) {
            List<Thread> threads = new ArrayList<>();
            for (int c = 0; c < consumers; c++) {
                Thread t = new Thread(new QueueConsumer(c), "consumer-" + c);
                t.start();
                threads.add(t);
            }
            produce();
            emit(obj("type", "producer-done"));
            drain();
            for (Thread t : threads) t.join();
            groupOffsets();
        } else {
            List<Thread> threads = new ArrayList<>();
            for (int w = 0; w < workers; w++) {
                Thread t = new Thread(new Worker(w), "worker-" + w);
                t.start();
                threads.add(t);
            }
            for (Thread t : threads) t.join();
        }
        emit(obj("type", "workers-done"));
        finalRead();
        emit(obj("type", "done"));
        sink.flush();
        sleep(1000);
        System.exit(0);
    }

    static final class AppInfoVersion {
        static String get() {
            try {
                return org.apache.kafka.common.utils.AppInfoParser.getVersion();
            } catch (Throwable t) {
                return "unknown";
            }
        }
    }
}
