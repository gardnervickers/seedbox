#!/usr/bin/env bash
# Train with Kafka's own launch classpath and the image's JVM, not Apache's JVM.
set -euo pipefail
work=$(mktemp -d)
pid=
cleanup() {
    if [[ -n "$pid" ]]; then
        kill -TERM "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
export KAFKA_NODE_ID=1
export KAFKA_CONTROLLER_QUORUM_VOTERS=1@localhost:9093
export KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://localhost:9092
export KAFKA_LOG_DIRS="$work/logs"
export KAFKA_JMX_OPTS="-Dcom.sun.management.jmxremote=true -Dcom.sun.management.jmxremote.authenticate=false -Dcom.sun.management.jmxremote.ssl=false"

# Dynamic archives layer on the Temurin default archive, which must also map.
export KAFKA_JVM_PERFORMANCE_OPTS="$KAFKA_JVM_PERFORMANCE_OPTS -Xshare:on -XX:ArchiveClassesAtExit=/opt/kafka/storage.jsa"
/opt/kafka/bin/kafka-run-class.sh kafka.docker.KafkaDockerWrapper setup \
    --default-configs-dir /etc/kafka/docker \
    --mounted-configs-dir /mnt/shared/config \
    --final-configs-dir "$work"
export KAFKA_JVM_PERFORMANCE_OPTS="${KAFKA_JVM_PERFORMANCE_OPTS% -XX:ArchiveClassesAtExit=*} -XX:ArchiveClassesAtExit=/opt/kafka/kafka.jsa"
/opt/kafka/bin/kafka-server-start.sh "$work/server.properties" > "$work/startup.log" 2>&1 &
pid=$!
# Startup must succeed; do not silently ship an archive trained on a failure.
for ((attempt=0; attempt<120; attempt++)); do
    if grep -q 'Kafka Server started' "$work/startup.log"; then
        kill -TERM "$pid"
        status=0
        wait "$pid" || status=$?
        [[ "$status" == 0 || "$status" == 143 ]]
        pid=
        test -s /opt/kafka/storage.jsa
        test -s /opt/kafka/kafka.jsa
        exit 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
        cat "$work/startup.log"
        exit 1
    fi
    sleep 1
done
cat "$work/startup.log"
echo 'Kafka CDS training did not start within 120 seconds' >&2
exit 1
