# Compiles the generated corpus with the JDK, then runs it on the glibc JRE image the Kafka work uses.
FROM docker.io/library/eclipse-temurin:21-jdk-jammy AS compile
COPY src /src
RUN mkdir /classes && javac --release 21 --add-modules jdk.incubator.vector -nowarn -XDsuppressNotes \
        -d /classes /src/*.java 2>&1 | grep -v -e '^warning' -e 'incubator' -e '^1 warning' || true; \
    test -f /classes/Fz.class

FROM docker.io/library/eclipse-temurin:21-jre-jammy
COPY --from=compile /classes /fuzz/classes
COPY batches /fuzz/batches
COPY run-batch.sh /fuzz/run-batch.sh
ENTRYPOINT ["/fuzz/run-batch.sh"]
