# Run Kafka's distribution and startup scripts on a glibc JVM.
FROM docker.io/apache/kafka:4.1.0 AS kafka
FROM docker.io/library/eclipse-temurin:21-jre
COPY --from=kafka /opt/kafka /opt/kafka
COPY --from=kafka /etc/kafka/docker /etc/kafka/docker
# These class-data archives belong to the original image's JVM.
RUN rm -f /opt/kafka/*.jsa
# One guest CPU: bound heap size and avoid parallel GC worker overhead.
ENV CLUSTER_ID=MkU3OEVBNTcwNTJENDM2Qk \
    KAFKA_PROCESS_ROLES=broker,controller \
    KAFKA_CONTROLLER_QUORUM_VOTERS=1@k1:9093,2@k2:9093,3@k3:9093 \
    KAFKA_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 \
    KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT \
    KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
    KAFKA_INTER_BROKER_LISTENER_NAME=PLAINTEXT \
    KAFKA_LOG_DIRS=/tmp/kraft-combined-logs \
    KAFKA_HEAP_OPTS="-Xms512m -Xmx512m" \
    KAFKA_JVM_PERFORMANCE_OPTS="-XX:+UseSerialGC -XX:ActiveProcessorCount=1"
# Generate both script-selected AppCDS archives on this exact JVM. Training
# uses a throwaway single-node cluster and leaves the runtime config unchanged.
COPY build-cds.sh /tmp/build-cds.sh
RUN timeout 180 bash /tmp/build-cds.sh && rm /tmp/build-cds.sh
ENTRYPOINT ["/etc/kafka/docker/run"]
