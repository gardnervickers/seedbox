# Compiles the generated corpus with the JDK, then runs it on the glibc JRE image the Kafka work uses.
FROM docker.io/library/eclipse-temurin:21-jdk-jammy AS compile
COPY src /src
RUN mkdir /classes && javac --release 21 --add-modules jdk.incubator.vector -nowarn -XDsuppressNotes \
        -d /classes /src/*.java 2>&1 | grep -v -e '^warning' -e 'incubator' -e '^1 warning' || true; \
    test -f /classes/Fz.class
# Preview classes run only with --enable-preview; compile them apart so the rest do not.
RUN if ls /src/preview/*.java > /dev/null 2>&1; then \
        javac --release 21 --enable-preview -XDsuppressNotes -cp /classes -d /classes /src/preview/*.java; fi
# Agent.class as a Java agent jar for Redefine (-javaagent:/fuzz/classes/agent.jar).
RUN printf 'Premain-Class: Agent\nCan-Redefine-Classes: true\nCan-Retransform-Classes: true\n' > /tmp/manifest && \
    jar --create --file /classes/agent.jar --manifest /tmp/manifest -C /classes Agent.class
# HotSpot's WhiteBox test API, at the JDK 21 update the runtime image ships, on the boot class
# path of WhiteBox probes (-Xbootclasspath/a:/fuzz/wb).
ADD --checksum=sha256:94b4c9a481e764e3b72287a59a1f06987586205a5f2cc5e17c3577996949d287 \
    https://raw.githubusercontent.com/openjdk/jdk21u/jdk-21.0.12-ga/test/lib/jdk/test/whitebox/WhiteBox.java /wbsrc/
ADD --checksum=sha256:f807936a68041072ca4a8b217e279890e50cbbcca2874488ed11dea70e2952e7 \
    https://raw.githubusercontent.com/openjdk/jdk21u/jdk-21.0.12-ga/test/lib/jdk/test/whitebox/parser/DiagnosticCommand.java /wbsrc/
RUN mkdir /wb && javac --release 21 -nowarn -XDsuppressNotes -d /wb /wbsrc/*.java && \
    if ls /src/whitebox/*.java > /dev/null 2>&1; then \
        javac --release 21 -XDsuppressNotes -cp /classes:/wb -d /classes /src/whitebox/*.java; fi

# Native probes, built against the runtime image's glibc (Ubuntu 22.04).
FROM docker.io/library/eclipse-temurin:21-jdk-jammy AS native
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends gcc libc6-dev > /dev/null
COPY src/c /src/c
RUN mkdir /native && gcc -O2 -pthread -o /native/cfz /src/c/cfz.c -ldl && \
    gcc -O2 -shared -fPIC -o /native/libfzdl.so /src/c/fzdl.c

FROM docker.io/library/eclipse-temurin:21-jre-jammy
COPY --from=compile /classes /fuzz/classes
COPY --from=compile /wb /fuzz/wb
COPY --from=native /native /fuzz/bin
COPY batches /fuzz/batches
COPY run-batch.sh /fuzz/run-batch.sh
ENTRYPOINT ["/fuzz/run-batch.sh"]
