# Apache Kafka 4.3.1 broker on a glibc JRE; the Apache image's own JRE is musl-based.
FROM docker.io/apache/kafka:4.3.1@sha256:77e3df9054047a88b520d0cc46e16696d3b22022e1d580aeccd2632df6532837 AS distribution
FROM docker.io/library/eclipse-temurin:21-jre-jammy@sha256:f04fb34e053148344e83317976114ec3f37e4b830ec8bdab5a2fe3cecd7d010b
COPY --from=distribution /opt/kafka /opt/kafka
# The class-data archives belong to the Apache image's different JDK build.
RUN rm -f /opt/kafka/kafka.jsa /opt/kafka/storage.jsa
COPY start-broker.sh /
COPY kafka-log4j.properties kafka-log4j2.properties /etc/
ENTRYPOINT ["/bin/bash", "/start-broker.sh"]
