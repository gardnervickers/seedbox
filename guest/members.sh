#!/bin/busybox sh
# The COM2 helper sources this file and owns every etcd child it starts.
start_member() {
 member="$1"; i="${member#n}"
 [ ! -f "/tmp/$member.pid" ] || { echo "$member already launched" >&2; return 1; }
 ip netns exec "$member" /bin/etcd --name "$member" --data-dir "/data/$member" \
  --listen-client-urls "http://10.0.$i.1:2379" --advertise-client-urls "http://10.0.$i.1:2379" \
  --listen-peer-urls "http://10.0.$i.1:2380" --initial-advertise-peer-urls "http://10.0.$i.1:2380" \
  --initial-cluster "$peers" --initial-cluster-state new --logger zap --log-level error >> "/tmp/$member.log" 2>&1 &
 pid=$!
 echo "$pid" > "/tmp/$member.pid"
 # Detect PID reuse if the shell has already reaped a spontaneous exit.
 awk '{print $22}' "/proc/$pid/stat" > "/tmp/$member.start" || :
}
stop_member() {
 member="$1"
 [ -f "/tmp/$member.pid" ] || { echo "$member not launched" >&2; return 1; }
 pid=$(cat "/tmp/$member.pid")
 if [ -r "/proc/$pid/stat" ] && [ "$(awk '{print $22}' "/proc/$pid/stat")" = "$(cat "/tmp/$member.start")" ]; then
  kill -9 "$pid" 2>/dev/null || :
 fi
 # Reap before ack/restart: wait completes only after file locks are released.
 wait "$pid" 2>/dev/null || :
 rm -f "/tmp/$member.pid" "/tmp/$member.start"
}
fault_ack() { echo "FAULT $* guest_ns=$(sbx time)" > /dev/ttyS1; }
