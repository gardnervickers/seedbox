#!/usr/bin/env bash
# One KRaft node. Environment: KAFKA_NODE_ID, CLUSTER_ID, KAFKA_VOTERS
# (e.g. "1@n1:9093,2@n2:9093,3@n3:9093"), KAFKA_RF (replication factor for internal
# topics and the default), optional KAFKA_ROLES (default "broker,controller"; or
# "broker" or "controller" for a separate quorum), KAFKA_HOST (advertised name,
# default nNODE_ID), KAFKA_SUPERVISE=1 (restart Kafka 2 s after it exits, as a
# service manager would: a broker halts when its only log directory fails) and
# SEEDBOX_EXTRA_PROPERTIES (';'-separated).
set -euo pipefail
rf=${KAFKA_RF:-3}
isr=$(( rf > 1 ? 2 : 1 ))
roles=${KAFKA_ROLES:-broker,controller}
host=${KAFKA_HOST:-n${KAFKA_NODE_ID}}
mkdir -p /data/kafka
case "$roles" in
  controller)
    listeners="listeners=CONTROLLER://:9093
advertised.listeners=CONTROLLER://${host}:9093
listener.security.protocol.map=CONTROLLER:PLAINTEXT" ;;
  broker)
    listeners="listeners=PLAINTEXT://:9092
advertised.listeners=PLAINTEXT://${host}:9092
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
inter.broker.listener.name=PLAINTEXT" ;;
  *)
    listeners="listeners=PLAINTEXT://:9092,CONTROLLER://:9093
advertised.listeners=PLAINTEXT://${host}:9092
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
inter.broker.listener.name=PLAINTEXT" ;;
esac
cat > /tmp/server.properties <<PROPS
process.roles=${roles}
node.id=${KAFKA_NODE_ID}
controller.quorum.voters=${KAFKA_VOTERS}
${listeners}
controller.listener.names=CONTROLLER
log.dirs=/data/kafka
auto.create.topics.enable=false
unclean.leader.election.enable=false
num.partitions=1
default.replication.factor=${rf}
min.insync.replicas=${isr}
offsets.topic.replication.factor=${rf}
offsets.topic.num.partitions=1
transaction.state.log.replication.factor=${rf}
transaction.state.log.min.isr=${isr}
transaction.state.log.num.partitions=1
group.initial.rebalance.delay.ms=0
PROPS
if [ -n "${SEEDBOX_EXTRA_PROPERTIES:-}" ]; then
  printf '%s\n' "${SEEDBOX_EXTRA_PROPERTIES}" | tr ';' '\n' >> /tmp/server.properties
fi
# Console logging only, with the transaction coordinator and transaction state
# transitions at DEBUG: its EndTxn handling is the evidence for KAFKA-17754.
if [ -f /opt/kafka/config/log4j2.yaml ]; then
  export KAFKA_LOG4J_OPTS="-Dlog4j2.configurationFile=/etc/kafka-log4j2.properties"
else
  export KAFKA_LOG4J_OPTS="-Dlog4j.configuration=file:/etc/kafka-log4j.properties"
fi
if [ ! -f /data/kafka/meta.properties ]; then
  /opt/kafka/bin/kafka-storage.sh format --ignore-formatted \
    --cluster-id "${CLUSTER_ID}" --config /tmp/server.properties
  # meta.properties and bootstrap.checkpoint must survive a power loss.
  sync
fi
if [ "${KAFKA_SUPERVISE:-0}" != 1 ]; then
  exec /opt/kafka/bin/kafka-server-start.sh /tmp/server.properties
fi
while true; do
  status=0
  /opt/kafka/bin/kafka-server-start.sh /tmp/server.properties || status=$?
  echo "@@KAFKA-EXITED status=${status}; restarting in 2 s"
  sleep 2
done
