# The Jepsen queue client with Kafka 3.8.0's kafka-clients jar and its runtime
# dependencies only, on a glibc JRE (the broker image is too large for a small machine).
FROM docker.io/apache/kafka:3.8.0@sha256:a6cdbbcf16c510f8e24c6b9937dd3418d03f8b14a8f2527cdba2998b59881d45 AS distribution
FROM docker.io/library/eclipse-temurin:21-jdk-jammy@sha256:e0c60c487345d1dc9d0fc7b6f0496f3cc941e5132e09296cc17a6decc71b902b AS build
COPY --from=distribution /opt/kafka/libs /opt/kafka/libs
COPY JepsenQueue.java /src/
RUN javac --release 17 -cp "/opt/kafka/libs/*" -d /client /src/JepsenQueue.java
FROM docker.io/library/eclipse-temurin:21-jre-jammy@sha256:f04fb34e053148344e83317976114ec3f37e4b830ec8bdab5a2fe3cecd7d010b
COPY --from=distribution /opt/kafka/libs/kafka-clients-3.8.0.jar /opt/kafka/libs/lz4-java-*.jar /opt/kafka/libs/snappy-java-*.jar /opt/kafka/libs/zstd-jni-*.jar /opt/kafka/libs/slf4j-*.jar /opt/kafka/libs/reload4j-*.jar /opt/kafka/libs/
COPY --from=build /client /client
COPY run-client.sh /
COPY client-log4j.properties client-log4j2.properties /etc/
ENTRYPOINT ["/bin/bash", "/run-client.sh"]
