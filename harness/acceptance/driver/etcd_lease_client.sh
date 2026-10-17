#!/bin/bash
# The lease holder for etcd-lease.yaml (etcd_lease.py explains the scenario).
# Grant a lease, attach two keys, publish the lease ID, then keep the lease
# alive with etcdctl, a real clientv3 gRPC client. When keep-alive gives up,
# record how it ended with a put that needs the client to reach the cluster
# again, then stay up.
set -u
endpoints=n1:2379,n2:2379,n3:2379
ctl() { etcdctl --endpoints="$endpoints" "$@"; }
until ctl endpoint health >/dev/null 2>&1; do sleep 0.5; done
lease=$(ctl lease grant 5 | awk '{print $2}')
ctl put --lease="$lease" lk/a 1 >/dev/null
ctl put --lease="$lease" lk/b 2 >/dev/null
ctl put lease-id "$lease" >/dev/null
echo "LEASE $lease"
ctl lease keep-alive "$lease"
ended="rc=$? uptime=$(cut -d' ' -f1 /proc/uptime)"
echo "KEEPALIVE_ENDED $ended"
until ctl put keepalive-ended "$ended" >/dev/null 2>&1; do sleep 0.5; done
echo "REPORTED $ended"
exec sleep infinity
