#!/bin/sh
# Run keel-s3dd with this script's arguments and start it again whenever it
# exits, as a production supervisor would: keel-s3dd ends the process by
# design (fail-stop) when its watchdog sees a wedged shard (SIGABRT, 134),
# when recovery refuses a frame that failed its checksum mid-log (3) and when
# its storage engine wedges on an I/O error (4). Before each restart
# it drops the page cache, as an operator would after a fail-stop, so a page
# read corrupt during recovery is read again from the disk. Each exit is
# logged.
while :; do
  /usr/local/bin/keel-s3dd "$@"
  echo "run-s3dd: keel-s3dd exited with status $?; restarting in 1s" >&2
  sync
  echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || echo "run-s3dd: could not drop the page cache" >&2
  sleep 1
done
