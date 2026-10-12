FROM docker.io/library/eclipse-temurin:21-jdk AS build
WORKDIR /source
COPY Workload.java .
RUN javac Workload.java
FROM docker.io/library/eclipse-temurin:21-jre
COPY --from=build /source/Workload.class /workload/Workload.class
ENTRYPOINT ["java", "-cp", "/workload", "Workload"]
