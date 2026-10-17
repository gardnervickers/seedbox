# Apache Kafka 3.8.0 broker on a glibc JRE; the Apache image's own JRE is musl-based.
FROM docker.io/apache/kafka:3.8.0@sha256:a6cdbbcf16c510f8e24c6b9937dd3418d03f8b14a8f2527cdba2998b59881d45 AS distribution
FROM docker.io/library/eclipse-temurin:21-jre-jammy@sha256:f04fb34e053148344e83317976114ec3f37e4b830ec8bdab5a2fe3cecd7d010b
COPY --from=distribution /opt/kafka /opt/kafka
# The class-data archives belong to the Apache image's different JDK build.
RUN rm -f /opt/kafka/kafka.jsa /opt/kafka/storage.jsa
COPY start-broker.sh /
COPY kafka-log4j.properties kafka-log4j2.properties /etc/
ENTRYPOINT ["/bin/bash", "/start-broker.sh"]
