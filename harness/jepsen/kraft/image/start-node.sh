#!/usr/bin/env bash
# One KRaft node: a dedicated controller or a dedicated broker.
# Environment:
#   KAFKA_NODE_ID, KAFKA_ROLE (controller|broker), KAFKA_HOST (advertised name),
#   CLUSTER_ID, QUORUM (static|dynamic);
#   static:  KAFKA_VOTERS ("1@k1:9093,2@k2:9093,...");
#   dynamic: KAFKA_BOOTSTRAP ("k1:9093,k2:9093,...") and, for an initial voter,
#            KAFKA_INITIAL_CONTROLLERS ("1@k1:9093:DIRID,..."); other nodes format
#            with --no-initial-controllers (brokers, and spare controllers that
#            join as observers until they are added);
#   optional SEEDBOX_EXTRA_PROPERTIES (';'-separated).
set -euo pipefail
mkdir -p /data/kafka
props=/tmp/node.properties
{
  echo "process.roles=${KAFKA_ROLE}"
  echo "node.id=${KAFKA_NODE_ID}"
  echo "log.dirs=/data/kafka"
  echo "controller.listener.names=CONTROLLER"
  if [ "${KAFKA_ROLE}" = controller ]; then
    echo "listeners=CONTROLLER://:9093"
    echo "advertised.listeners=CONTROLLER://${KAFKA_HOST}:9093"
    echo "listener.security.protocol.map=CONTROLLER:PLAINTEXT"
  else
    echo "listeners=PLAINTEXT://:9092"
    echo "advertised.listeners=PLAINTEXT://${KAFKA_HOST}:9092"
    echo "listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT"
    echo "inter.broker.listener.name=PLAINTEXT"
  fi
  if [ "${QUORUM}" = static ]; then
    echo "controller.quorum.voters=${KAFKA_VOTERS}"
  else
    echo "controller.quorum.bootstrap.servers=${KAFKA_BOOTSTRAP}"
  fi
  echo "auto.create.topics.enable=false"
  echo "num.partitions=1"
  echo "default.replication.factor=1"
  echo "offsets.topic.replication.factor=1"
  echo "offsets.topic.num.partitions=1"
  echo "transaction.state.log.replication.factor=1"
  echo "transaction.state.log.min.isr=1"
  echo "transaction.state.log.num.partitions=1"
  echo "group.initial.rebalance.delay.ms=0"
  # The cleaner's default 128 MiB dedupe buffer does not fit a small heap.
  echo "log.cleaner.dedupe.buffer.size=8388608"
} > "$props"
if [ -n "${SEEDBOX_EXTRA_PROPERTIES:-}" ]; then
  printf '%s\n' "${SEEDBOX_EXTRA_PROPERTIES}" | tr ';' '\n' >> "$props"
fi
export KAFKA_LOG4J_OPTS="-Dlog4j2.configurationFile=/etc/kafka-log4j2.properties"
format=(/opt/kafka/bin/kafka-storage.sh format --ignore-formatted
  --cluster-id "${CLUSTER_ID}" --config "$props")
if [ "${QUORUM}" = dynamic ]; then
  if [ -n "${KAFKA_INITIAL_CONTROLLERS:-}" ]; then
    format+=(--initial-controllers "${KAFKA_INITIAL_CONTROLLERS}")
  else
    format+=(--no-initial-controllers)
  fi
fi
if [ ! -f /data/kafka/meta.properties ]; then
  "${format[@]}"
  # meta.properties and the bootstrap checkpoint must survive a power loss.
  sync
fi
exec /opt/kafka/bin/kafka-server-start.sh "$props"
