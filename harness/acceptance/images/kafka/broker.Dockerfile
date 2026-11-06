# Kafka 3.9.1 on a glibc JRE; the Apache image's own JRE is musl-based.
FROM docker.io/apache/kafka:3.9.1@sha256:4ceccc577f03f51f6af8dbfda55194d0d892f4fa7913ffbded567ce3895622ed AS distribution
FROM docker.io/library/eclipse-temurin:21-jre-jammy@sha256:f04fb34e053148344e83317976114ec3f37e4b830ec8bdab5a2fe3cecd7d010b
COPY --from=distribution /opt/kafka /opt/kafka
# The class-data archives belong to the Apache image's different JDK build.
RUN rm -f /opt/kafka/kafka.jsa /opt/kafka/storage.jsa
COPY start-broker.sh /start-broker.sh
ENTRYPOINT ["/bin/bash", "/start-broker.sh"]
