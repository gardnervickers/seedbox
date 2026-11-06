"""Single-broker Kafka scenarios (long-running tier); kafka-python 2.2.20 clients.
Not named kafka.py: the script directory would shadow the kafka package.

Roles:
  startup-crash      oracle for kafka-startup-crash.yaml (broker startup, then durability)
  producer           phase-driven producer machine for kafka-idempotent-retry.yaml
  idempotent-retry   oracle for kafka-idempotent-retry.yaml

startup-crash, arm (i): the broker answers, a topic receives 200 acknowledged
records and a consumer reads exactly those, in order. Arm (ii): a topic with
topic-level flush.messages=1 and retention disabled, RF 1, min.insync.replicas 1.
After the initialization boundary (topic created, one record acknowledged and
read back, 5 simulated seconds idle) the oracle crashes the broker machine right
after 100 further acknowledgments, reboots it and requires every acknowledged
record exactly once, in order. With flush.messages=1, UnifiedLog.append flushes
the segment before the append returns (Kafka 3.9: `unflushedMessages >=
flushInterval`), so an acknowledgment implies the record reached the disk.

idempotent-retry: the producer sends numbered batches with an idempotent
producer. The oracle arms loss on every open broker-to-producer connection, so
the broker's response to one batch is dropped; the producer times out, opens a
new connection and resends the batch with the same sequence number. The oracle
heals, requires dropped packets, and the log must hold the logical sequence
exactly once.
"""
import json
import socket
import sys
import time

BOOTSTRAP = ["kafka:9092"]
CLIENT = dict(bootstrap_servers=BOOTSTRAP, api_version=(3, 9))


class Failed(RuntimeError):
    """The scenario's own check failed."""


def require(condition, message):
    if not condition:
        raise Failed(message)


def report(kind, **fields):
    print(json.dumps(dict(kind=kind, at_ns=time.monotonic_ns(), **fields), sort_keys=True), flush=True)


def until(deadline, what, attempt, pause=0.2):
    """Retry a retriable Kafka operation until it returns a value or the simulated deadline."""
    from kafka.errors import KafkaError
    last = None
    while time.monotonic() < deadline:
        try:
            value = attempt()
            if value is not None:
                return value
        except KafkaError as error:
            if not getattr(error, "retriable", False):
                raise
            last = error
        time.sleep(pause)
    raise Failed(f"{what} not reached before deadline: {last!r}")


def admin(deadline):
    from kafka.admin import KafkaAdminClient
    return until(deadline, "broker admin connection", lambda: KafkaAdminClient(
        client_id="seedbox-admin", request_timeout_ms=5000, **CLIENT), pause=0.5)


def create_topic(client, deadline, name, configs=None):
    from kafka.admin import NewTopic
    from kafka.errors import TopicAlreadyExistsError

    def attempt():
        try:
            client.create_topics([NewTopic(name, num_partitions=1, replication_factor=1,
                                           topic_configs=configs or {})], timeout_ms=10000)
        except TopicAlreadyExistsError:
            pass  # A lost response; the empty-log checks below still apply.
        return True
    until(deadline, f"create {name}", attempt)

    def described():
        topics = client.describe_topics([name])
        partitions = topics[0]["partitions"] if topics and topics[0]["error_code"] == 0 else []
        ready = partitions and partitions[0]["error_code"] == 0 and partitions[0]["leader"] == 1
        return topics if ready else None
    return until(deadline, f"{name} leader", described)


def topic_configs(client, name, keys):
    from kafka.admin import ConfigResource, ConfigResourceType
    found = {}
    for response in client.describe_configs([ConfigResource(ConfigResourceType.TOPIC, name, None)]):
        for _code, _message, _type, resource, entries in response.resources:
            require(resource == name, f"describe_configs returned {resource}")
            for entry in entries:
                key, value, _read_only = entry[0], entry[1], entry[2]
                if key in keys:
                    found[key] = value
    return found


def producer(client_id, **overrides):
    from kafka import KafkaProducer
    settings = dict(client_id=client_id, acks="all", enable_idempotence=True,
                    max_in_flight_requests_per_connection=1, linger_ms=0,
                    request_timeout_ms=10000, delivery_timeout_ms=120000, max_block_ms=30000,
                    allow_auto_create_topics=False)
    settings.update(overrides)
    return KafkaProducer(**CLIENT, **settings)


def value(seq):
    return f"seq-{seq:05d}"


def read_all(topic, deadline):
    """Read partition 0 from offset 0 to its current end; returns [(offset, key, value)]."""
    from kafka import KafkaConsumer, TopicPartition
    partition = TopicPartition(topic, 0)
    consumer = KafkaConsumer(**CLIENT, client_id="seedbox-reader", group_id=None,
                             enable_auto_commit=False, auto_offset_reset="none",
                             isolation_level="read_committed", request_timeout_ms=15000,
                             fetch_max_wait_ms=100, allow_auto_create_topics=False)
    try:
        consumer.assign([partition])
        start = until(deadline, f"{topic} beginning", lambda: consumer.beginning_offsets([partition])[partition])
        end = until(deadline, f"{topic} end", lambda: consumer.end_offsets([partition])[partition])
        require(start == 0, f"{topic} beginning offset {start}")
        consumer.seek(partition, 0)
        records = []
        while consumer.position(partition) < end:
            require(time.monotonic() < deadline, f"{topic} read did not reach offset {end}")
            for batch in consumer.poll(timeout_ms=200).values():
                records.extend((r.offset, r.key.decode(), r.value.decode()) for r in batch if r.offset < end)
        return records
    finally:
        consumer.close(autocommit=False)


def exactly(records, expected, topic):
    """The log holds exactly the expected values at offsets 0.. in order."""
    values = [record[2] for record in records]
    offsets = [record[0] for record in records]
    require(len(set(values)) == len(values), f"{topic}: duplicate records")
    require(values == expected, f"{topic}: log {values[:3]}..{values[-3:]} ({len(values)}) "
            f"differs from acknowledged {expected[:3]}..{expected[-3:]} ({len(expected)})")
    require(offsets == list(range(len(expected))), f"{topic}: offsets are not dense from 0")


def send_all(producer_, topic, seqs, deadline):
    """Send asynchronously, then require an acknowledgment for every record."""
    futures = [(seq, producer_.send(topic, key=f"k{seq % 8}".encode(), value=value(seq).encode()))
               for seq in seqs]
    acked = []
    for seq, future in futures:
        metadata = future.get(timeout=max(1, deadline - time.monotonic()))
        acked.append((seq, metadata.offset))
    return acked


def startup_crash():
    from seedbox import World
    deadline = time.monotonic() + 1200
    with World.connect() as world:
        # Arm (i): startup and 200 acknowledged records.
        client = admin(deadline)
        report("broker_ready")
        create_topic(client, deadline, "startup")
        first = producer("seedbox-startup")
        acked = send_all(first, "startup", range(200), deadline)
        first.close(timeout=10)
        require([offset for _, offset in acked] == list(range(200)), f"startup acknowledgments {acked[:3]}...")
        exactly(read_all("startup", deadline), [value(seq) for seq in range(200)], "startup")
        report("startup", acknowledged=200)

        # Arm (ii): durability across a machine crash.
        durable = {"flush.messages": "1", "retention.ms": "-1", "retention.bytes": "-1",
                   "min.insync.replicas": "1", "cleanup.policy": "delete"}
        create_topic(client, deadline, "durable", durable)
        effective = topic_configs(client, "durable", set(durable))
        require(effective == durable, f"effective topic configuration {effective}")
        client.close()
        writer = producer("seedbox-durable")
        acknowledged = send_all(writer, "durable", [0], deadline)
        exactly(read_all("durable", deadline), [value(0)], "durable")
        world.sleep(5)
        report("initialized", configuration=effective)
        for seq in range(1, 101):
            # One record per acknowledgment, so the crash follows the 100th exactly.
            acknowledged += send_all(writer, "durable", [seq], deadline)
        crash = world.crash("kafka")
        writer.close(timeout=0)
        require([offset for _, offset in acknowledged] == list(range(101)), "durable offsets before crash")
        report("crashed", acknowledged=len(acknowledged), effect_at_ns=crash.effect_at_ns)
        reboot = world.reboot("kafka")
        admin(deadline).close()
        report("rebooted", effect_at_ns=reboot.effect_at_ns)
        records = read_all("durable", deadline)
        exactly(records, [value(seq) for seq, _ in acknowledged], "durable")
        report("result", verdict="pass", startup_records=200, durable_records=len(records))


def producer_machine():
    """Phase server for the oracle; every phase replies with one JSON line."""
    deadline = time.monotonic() + 1200
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", 9000))
    listener.listen()
    channel, _ = listener.accept()
    reader = channel.makefile("rb")
    writer = None
    expected = []
    for line in reader:
        op = json.loads(line)
        if op["op"] == "start":
            client = admin(deadline)
            create_topic(client, deadline, "retry", {"min.insync.replicas": "1"})
            client.close()
            # A short request timeout makes the dropped response a retry, not a failure.
            writer = producer("seedbox-retry", request_timeout_ms=3000, retry_backoff_ms=100,
                              reconnect_backoff_ms=50)
            result = {"acked": send_all(writer, "retry", range(0, 10), deadline)}
        elif op["op"] == "batch":
            started = time.monotonic()
            seqs = range(op["first"], op["first"] + 10)
            result = {"acked": send_all(writer, "retry", seqs, deadline),
                      "elapsed_s": round(time.monotonic() - started, 3)}
        elif op["op"] == "verify":
            writer.close(timeout=10)
            result = {"log": [list(record) for record in read_all("retry", deadline)]}
        elif op["op"] == "done":
            return
        else:
            raise Failed(f"unknown phase {op}")
        channel.sendall(json.dumps(result).encode() + b"\n")


def idempotent_retry():
    from seedbox import World
    with World.connect() as world:
        channel = None
        for _ in range(600):
            try:
                channel = socket.create_connection(("producer", 9000), timeout=2)
                break
            except OSError:
                time.sleep(0.1)
        require(channel is not None, "producer phase server unreachable")
        channel.settimeout(900)  # Includes broker startup under the translator.
        reader = channel.makefile("rb")

        def phase(op, **fields):
            channel.sendall(json.dumps(dict(op=op, **fields)).encode() + b"\n")
            line = reader.readline()
            require(line, f"producer exited during {op}")
            return json.loads(line)

        acked = phase("start")["acked"]
        report("started", acknowledged=len(acked))
        flows = [flow for flow in world.connections("kafka", "producer")["connections"]
                 if flow["src_port"] == 9092]
        require(flows, "no open broker-to-producer connection")
        handles = [world.drop_connection(flow) for flow in flows]
        report("armed", connections=flows)
        faulted = phase("batch", first=10)
        evidence = [world.heal(handle) for handle in handles]
        dropped = sum(item["dropped"] for item in evidence)
        require(all(item["matched"] >= item["dropped"] for item in evidence), f"selection counts {evidence}")
        require(dropped > 0, f"no broker response was dropped: {evidence}")
        # The lost response forces at least one request timeout (3 s) and a resend.
        require(faulted["elapsed_s"] >= 3, f"faulted batch completed without a retry: {faulted}")
        report("healed", faulted_batch=faulted, selection_counts=evidence)
        acked += faulted["acked"] + phase("batch", first=20)["acked"]
        log = phase("verify")["log"]
        channel.sendall(b'{"op": "done"}\n')
        require([offset for _, offset in acked] == list(range(30)), f"acknowledged offsets {acked}")
        exactly([tuple(record) for record in log], [value(seq) for seq in range(30)], "retry")
        report("result", verdict="pass", records=len(log), dropped=dropped,
               faulted_batch_elapsed_s=faulted["elapsed_s"])


def main():
    roles = {"startup-crash": startup_crash, "producer": producer_machine,
             "idempotent-retry": idempotent_retry}
    role = roles.get(sys.argv[1] if len(sys.argv) == 2 else None)
    if role is None:
        sys.exit("usage: kafka_broker.py " + "|".join(roles))
    role()


if __name__ == "__main__":
    main()
