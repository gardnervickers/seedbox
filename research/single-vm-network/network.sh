#!/bin/busybox sh
# Source into PID1. Root is a router; each service owns a distinct network stack.
setup_network() {
 mkdir -p /run/netns /var/run
 ip link set lo up
 echo 1 > /proc/sys/net/ipv4/ip_forward
 for i in 1 2 3 4; do
  ns=n$i
  ip netns add "$ns"
  ip link add "host$i" type veth peer name "svc$i"
  ip link set "svc$i" netns "$ns"
  ip addr add "10.0.$i.2/30" dev "host$i"
  ip link set "host$i" up
  ip netns exec "$ns" ip link set lo up
  ip netns exec "$ns" ip link set "svc$i" name eth0
  ip netns exec "$ns" ip addr add "10.0.$i.1/30" dev eth0
  ip netns exec "$ns" ip link set eth0 up
  ip netns exec "$ns" ip route add default via "10.0.$i.2"
 done
}
set_delay() {
 for i in 1 2 3 4; do
  tc qdisc replace dev "host$i" root netem delay "$1" limit 10000
 done
}
partition() {
 i="$1"
 # Destination direction: drop all packets from router into service.
 tc qdisc replace dev "host$i" root netem delay 1ms loss 100% limit 10000
 # Source direction: drop service traffic on entry into router.
 tc qdisc add dev "host$i" ingress
 tc filter add dev "host$i" parent ffff: protocol all pref 1 matchall action drop
 echo "NETWORK partition n$i"
}
heal() {
 i="$1"
 tc qdisc del dev "host$i" ingress
 tc qdisc replace dev "host$i" root netem delay 1ms limit 10000
 echo "NETWORK heal n$i"
}
network_controls() {
 for i in 1 2 3; do
  ip netns exec n$i /bin/netprobe serve > /tmp/probe$i.log 2>&1 &
  echo "ISOLATION n$i $(ip netns exec n$i readlink /proc/self/ns/net)"
 done
 sleep .01
 ip netns exec n1 /bin/netprobe bind-deny 10.0.2.1
 echo "NETWORK warm-arp"
 ip netns exec n1 /bin/netprobe probe 10.0.2.1 pass 0
 echo "NETWORK zero-delay"
 for sample in 1 2 3; do ip netns exec n1 /bin/netprobe probe 10.0.2.1 pass 0; done
 set_delay 1ms
 for i in 1 2 3 4; do echo "NETWORK qdisc host$i"; tc -s qdisc show dev "host$i"; done
 echo "NETWORK fixed-delay one-ms-per-direction"
 for sample in 1 2 3; do ip netns exec n1 /bin/netprobe probe 10.0.2.1 pass 2000000; done
 partition 1
 ip netns exec n1 /bin/netprobe probe 10.0.2.1 blocked 0
 ip netns exec n2 /bin/netprobe probe 10.0.1.1 blocked 0
 ip netns exec n2 /bin/netprobe probe 10.0.3.1 pass 2000000
 heal 1
 ip netns exec n1 /bin/netprobe probe 10.0.2.1 pass 2000000
 ip netns exec n2 /bin/netprobe probe 10.0.1.1 pass 2000000
 tc -s qdisc show dev host1
 echo "NETWORK controls-pass"
}
