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
    gcc -O2 -shared -fPIC -o /native/libfzdl.so /src/c/fzdl.c && \
    gcc -O2 -static-pie -o /native/cfzpie /src/c/cfzpie.c

# Probes for other runtimes (probes/go, js, cs, erl, lua). Each runtime comes from an image pinned
# by digest or a source archive pinned by checksum, and runs on the runtime image's glibc 2.35.
# Go: statically linked, with its own runtime, signal handling and collector.
FROM docker.io/library/golang:1.27.0-bookworm@sha256:ded31c68586d2e49e760acc2e65a884b23d032e9bbbed0ae0c55abd3fcaf4452 AS go
COPY src/go /src/go
RUN cd /src/go && CGO_ENABLED=0 GOTOOLCHAIN=local go build -trimpath -o /out/gofz .

# Node.js: the official linux-arm64 build (needs glibc 2.28), as the node image installs it.
FROM docker.io/library/node:24.20.0-bookworm-slim@sha256:ba849c60be29959425b8734d57b8b4b7d56f98edd9504c9af091d5281095a71e AS node

# .NET: framework-dependent and not ReadyToRun, so the probe starts at tier 0; the runtime image
# takes the shared runtime from the same SDK. Publishing needs no package downloads.
FROM mcr.microsoft.com/dotnet/sdk:10.0.400@sha256:4beef5b8919dcaa2dc924233bd069257e883cc7a061e09088a97d152d6a48510 AS dotnet
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
COPY src/cs /src/cs
RUN --network=none dotnet publish /src/cs -c Release -r linux-arm64 --self-contained false -o /out/cs -v quiet && \
    rm /out/cs/*.pdb

# Erlang/OTP with the AArch64 BEAM JIT, built for Ubuntu 22.04; only the emulator, kernel and
# stdlib are kept.
FROM docker.io/hexpm/erlang:28.5.0.6-ubuntu-jammy-20260810@sha256:32587e1dc2225051aa67d25725e0cd9566f990a4eff7d9b27aa9dd46e53bded0 AS erlang
RUN mkdir -p /otp/lib && cd /usr/local/lib/erlang && cp -a bin erts-* releases /otp/ && \
    cp -a lib/kernel-* lib/stdlib-* /otp/lib/ && rm -rf /otp/erts-*/doc /otp/erts-*/include /otp/lib/*/src
COPY src/erl /src/erl
RUN mkdir /out && erlc -o /out /src/erl/beamfz.erl

# LuaJIT from a pinned commit of the v2.1 branch.
FROM docker.io/library/ubuntu:22.04@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401 AS luajit
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends gcc libc6-dev make > /dev/null
ADD --checksum=sha256:178c656b62bb796e536a23f1d3bb09e1824e4dd28eaf2a4758b295ac328e32e6 \
    https://github.com/LuaJIT/LuaJIT/archive/24c20c94e7db195b640854619577441f9b4bc6be.tar.gz /luajit.tar.gz
RUN mkdir /luajit && tar -xzf /luajit.tar.gz -C /luajit --strip-components=1 && make -C /luajit/src -j4 -s && \
    mkdir /out && cp /luajit/src/luajit /out/

FROM docker.io/library/eclipse-temurin:21-jre-jammy
COPY --from=node /usr/local/bin/node /fuzz/node/node
COPY --from=dotnet /usr/share/dotnet/dotnet /usr/share/dotnet/dotnet
COPY --from=dotnet /usr/share/dotnet/host /usr/share/dotnet/host
COPY --from=dotnet /usr/share/dotnet/shared/Microsoft.NETCore.App /usr/share/dotnet/shared/Microsoft.NETCore.App
COPY --from=erlang /otp /usr/local/lib/erlang
COPY --from=luajit /out/luajit /fuzz/luajit/luajit
COPY --from=compile /classes /fuzz/classes
COPY --from=compile /wb /fuzz/wb
COPY --from=native /native /fuzz/bin
COPY --from=go /out/gofz /fuzz/bin/
COPY --from=dotnet /out/cs /fuzz/bin/
COPY --from=erlang /out /fuzz/erl
COPY src/js/jsfz.js /fuzz/js/
COPY src/lua/luafz.lua /fuzz/lua/
COPY --chmod=755 src/js/jsfz src/erl/erlfz src/lua/luafz /fuzz/bin/
COPY batches /fuzz/batches
COPY run-batch.sh /fuzz/run-batch.sh
ENTRYPOINT ["/fuzz/run-batch.sh"]
