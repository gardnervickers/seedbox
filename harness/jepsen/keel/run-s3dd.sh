#!/bin/sh
# Run keel-s3dd with this script's arguments and start it again whenever it
# exits, as a production supervisor would: keel-s3dd's watchdog and fatal
# paths end the process by design (fail-stop). Each exit is logged.
while :; do
  /usr/local/bin/keel-s3dd "$@"
  echo "run-s3dd: keel-s3dd exited with status $?; restarting in 1s" >&2
  sleep 1
done
