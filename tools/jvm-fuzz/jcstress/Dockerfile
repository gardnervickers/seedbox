# Builds OpenJDK jcstress from its release tag (JDK 17: 0.16 fails -Werror on JDK 21) and runs it on the same glibc JRE as the corpus.
# Build args: SUITES is a Maven module list (jcstress-samples, tests-all, tests-custom).
FROM docker.io/library/maven:3.9-eclipse-temurin-17 AS build
ARG JCSTRESS_REF=0.16
ARG SUITES=jcstress-samples
RUN git clone --depth 1 --branch "$JCSTRESS_REF" https://github.com/openjdk/jcstress.git /jcstress
WORKDIR /jcstress
COPY oversubscribe.patch /tmp/
RUN git apply /tmp/oversubscribe.patch
RUN mvn -B -q -pl "$SUITES" -am package -DskipTests && mkdir /out && \
    for s in $(echo "$SUITES" | tr ',' ' '); do cp "$s"/target/jcstress.jar "/out/$s.jar"; done

FROM docker.io/library/eclipse-temurin:21-jre-jammy
COPY --from=build /out /jcstress
COPY run-jcstress.sh /jcstress/run-jcstress.sh
WORKDIR /tmp
ENTRYPOINT ["/jcstress/run-jcstress.sh"]
