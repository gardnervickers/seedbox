# The Jepsen queue client with Kafka 4.3.1's kafka-clients jar and its runtime
# dependencies only, on a glibc JRE (the broker image is too large for a small machine).
FROM docker.io/apache/kafka:4.3.1@sha256:77e3df9054047a88b520d0cc46e16696d3b22022e1d580aeccd2632df6532837 AS distribution
FROM docker.io/library/eclipse-temurin:21-jdk-jammy@sha256:e0c60c487345d1dc9d0fc7b6f0496f3cc941e5132e09296cc17a6decc71b902b AS build
COPY --from=distribution /opt/kafka/libs /opt/kafka/libs
COPY JepsenQueue.java /src/
RUN javac --release 17 -cp "/opt/kafka/libs/*" -d /client /src/JepsenQueue.java
FROM docker.io/library/eclipse-temurin:21-jre-jammy@sha256:f04fb34e053148344e83317976114ec3f37e4b830ec8bdab5a2fe3cecd7d010b
COPY --from=distribution /opt/kafka/libs/kafka-clients-4.3.1.jar /opt/kafka/libs/lz4-java-*.jar /opt/kafka/libs/snappy-java-*.jar /opt/kafka/libs/zstd-jni-*.jar /opt/kafka/libs/slf4j-*.jar /opt/kafka/libs/log4j-api-*.jar /opt/kafka/libs/log4j-core-*.jar /opt/kafka/libs/log4j-slf4j-impl-*.jar /opt/kafka/libs/
COPY --from=build /client /client
COPY run-client.sh /
COPY client-log4j.properties client-log4j2.properties /etc/
ENTRYPOINT ["/bin/bash", "/run-client.sh"]
