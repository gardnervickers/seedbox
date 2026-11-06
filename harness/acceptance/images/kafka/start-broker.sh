#!/usr/bin/env bash
# Single combined broker+controller; data and KRaft metadata live in /data.
set -euo pipefail
mkdir -p /data/kafka
cat > /tmp/server.properties <<EOF
process.roles=broker,controller
node.id=1
controller.quorum.voters=1@kafka:9093
listeners=PLAINTEXT://:9092,CONTROLLER://:9093
advertised.listeners=PLAINTEXT://kafka:9092
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
controller.listener.names=CONTROLLER
inter.broker.listener.name=PLAINTEXT
log.dirs=/data/kafka
auto.create.topics.enable=false
num.partitions=1
default.replication.factor=1
min.insync.replicas=1
offsets.topic.replication.factor=1
offsets.topic.num.partitions=1
transaction.state.log.replication.factor=1
transaction.state.log.min.isr=1
group.initial.rebalance.delay.ms=0
EOF
/opt/kafka/bin/kafka-storage.sh format --ignore-formatted \
  --cluster-id 4L6g3nShT-eMCtK--X86sw --config /tmp/server.properties
# A crash discards unflushed volume writes; a rebooted broker skips formatting
# but must read meta.properties and bootstrap.checkpoint.
sync
exec /opt/kafka/bin/kafka-server-start.sh /tmp/server.properties
