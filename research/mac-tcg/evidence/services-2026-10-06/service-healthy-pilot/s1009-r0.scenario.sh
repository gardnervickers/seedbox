set -eu
exec 3>/run/seedbox-control
epoch="$1"
target=$((epoch + 750765185))
/bin/tcg-bootstrap "$target"
echo "REQUEST partition n1 n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "partition n1 n2" >&3
target=$((epoch + 1450765185))
/bin/tcg-bootstrap "$target"
echo "REQUEST heal n1 n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "heal n1 n2" >&3
target=$((epoch + 1850765185))
/bin/tcg-bootstrap "$target"
echo "REQUEST stop n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "stop n2" >&3
target=$((epoch + 2450765185))
/bin/tcg-bootstrap "$target"
echo "REQUEST start n2 target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1
printf "%s\n" "start n2" >&3
touch /tmp/scenario.done
while :; do sleep 1; done
