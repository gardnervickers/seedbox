set -eu
exec 3>/run/seedbox-control
epoch="$1"
target=$((epoch + 720156336))
/bin/tcg-bootstrap "$target"
echo "REQUEST partition n1 n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "partition n1 n2" >&3
target=$((epoch + 1420156336))
/bin/tcg-bootstrap "$target"
echo "DROP_COUNTER_BEGIN" > /dev/ttyS1
tc -s filter show dev host1 parent ffff: > /dev/ttyS1
echo "DROP_COUNTER_END" > /dev/ttyS1
echo "REQUEST heal n1 n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "heal n1 n2" >&3
target=$((epoch + 1820156336))
/bin/tcg-bootstrap "$target"
echo "REQUEST stop n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "stop n2" >&3
target=$((epoch + 2420156336))
/bin/tcg-bootstrap "$target"
echo "REQUEST start n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "start n2" >&3
touch /tmp/scenario.done
while :; do sleep 1; done
