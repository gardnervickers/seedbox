# Apache Kafka 4.3.1 (KRaft) on a small glibc runtime. A Seedbox guest's root
# filesystem is RAM, so the image drops Kafka Streams, Connect and their web
# stack (none run here) and uses a jlinked runtime of the modules Kafka needs,
# built from Temurin's JDK 21 (the Apache image's own JRE is musl-based).
FROM docker.io/apache/kafka:4.3.1@sha256:77e3df9054047a88b520d0cc46e16696d3b22022e1d580aeccd2632df6532837 AS distribution
FROM docker.io/library/eclipse-temurin:21-jdk-jammy@sha256:e0c60c487345d1dc9d0fc7b6f0496f3cc941e5132e09296cc17a6decc71b902b AS runtime
COPY --from=distribution /opt/kafka /opt/kafka
RUN cd /opt/kafka && rm -rf site-docs bin/windows kafka.jsa storage.jsa \
 && cd libs && rm -f connect-*.jar kafka-streams*.jar rocksdbjni-*.jar jetty-*.jar jersey-*.jar \
      hk2-*.jar trogdor-*.jar jakarta.*.jar jackson-jakarta-*.jar jackson-module-jakarta-*.jar \
      javassist-*.jar swagger-*.jar osgi-*.jar aopalliance-*.jar classgraph-*.jar jline-*.jar \
      maven-artifact-*.jar plexus-utils-*.jar \
 && jlink --add-modules java.base,java.compiler,java.desktop,java.instrument,java.logging,java.management,java.management.rmi,java.naming,java.net.http,java.rmi,java.scripting,java.security.jgss,java.security.sasl,java.sql,java.transaction.xa,java.xml,jdk.crypto.ec,jdk.management,jdk.management.agent,jdk.jfr,jdk.naming.dns,jdk.net,jdk.security.auth,jdk.security.jgss,jdk.unsupported,jdk.zipfs \
      --strip-debug --no-man-pages --no-header-files --compress=zip-6 --output /jre
FROM docker.io/library/ubuntu:22.04@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401
COPY --from=runtime /jre /opt/java
COPY --from=runtime /opt/kafka /opt/kafka
ENV JAVA_HOME=/opt/java PATH=/opt/java/bin:$PATH
COPY start-node.sh /
COPY log4j2.properties /etc/kafka-log4j2.properties
ENTRYPOINT ["/bin/bash", "/start-node.sh"]
