#!/usr/bin/env bash
# Start the Jepsen queue client with the image's own kafka-clients jar.
if ls /opt/kafka/libs/log4j-core-*.jar >/dev/null 2>&1; then
  LOG="-Dlog4j2.configurationFile=/etc/client-log4j2.properties"
else
  LOG="-Dlog4j.configuration=file:/etc/client-log4j.properties"
fi
exec java -Xms64m -Xmx192m -XX:+UseSerialGC -XX:TieredStopAtLevel=1 -XX:CICompilerCount=1 -XX:-UsePerfData \
  $LOG -cp "/client:/opt/kafka/libs/*" JepsenQueue "$@"
