#!/usr/bin/env bash
# One KRaft broker+controller. Environment: KAFKA_NODE_ID, CLUSTER_ID, KAFKA_VOTERS
# (e.g. "1@n1:9093,2@n2:9093,3@n3:9093"), KAFKA_RF (replication factor for internal
# topics and the default), optional SEEDBOX_EXTRA_PROPERTIES (';'-separated).
set -euo pipefail
rf=${KAFKA_RF:-3}
isr=$(( rf > 1 ? 2 : 1 ))
mkdir -p /data/kafka
cat > /tmp/server.properties <<PROPS
process.roles=broker,controller
node.id=${KAFKA_NODE_ID}
controller.quorum.voters=${KAFKA_VOTERS}
listeners=PLAINTEXT://:9092,CONTROLLER://:9093
advertised.listeners=PLAINTEXT://n${KAFKA_NODE_ID}:9092
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
controller.listener.names=CONTROLLER
inter.broker.listener.name=PLAINTEXT
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
/opt/kafka/bin/kafka-storage.sh format --ignore-formatted \
  --cluster-id "${CLUSTER_ID}" --config /tmp/server.properties
# meta.properties and bootstrap.checkpoint must survive a power loss.
sync
exec /opt/kafka/bin/kafka-server-start.sh /tmp/server.properties
