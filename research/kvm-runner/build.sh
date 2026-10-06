#!/usr/bin/env bash
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
SEEDBOX_STATIC_CC="${SEEDBOX_STATIC_CC%/cc}/x86_64-unknown-linux-musl-cc"
src=$root/linux-source/linux-6.12.112
python3 "$root/prepare.py"
patch -d "$src" -p1 < "$root/hooks.patch"
mkdir -p "$root/kernel-out"
make -C "$src" O="$root/kernel-out" allnoconfig
while read -r state option; do
 case "$state" in y) "$src/scripts/config" --file "$root/kernel-out/.config" --enable "$option";; n) "$src/scripts/config" --file "$root/kernel-out/.config" --disable "$option";; esac
done < "$root/config.options"
for option in INET PACKET VIRTIO VIRTIO_MMIO VIRTIO_NET NETDEVICES NET_CORE ETHERTAP; do "$src/scripts/config" --file "$root/kernel-out/.config" --enable "$option"; done
make -C "$src" O="$root/kernel-out" olddefconfig
export KBUILD_BUILD_USER=seedbox KBUILD_BUILD_HOST=probe KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970' SOURCE_DATE_EPOCH=0
make -C "$src" O="$root/kernel-out" -j8 bzImage
cp "$root/kernel-out/arch/x86/boot/bzImage" "$root/inputs/clock-kernel"
if [ ! -d "$root/redis-7.2.7" ]; then
 curl -L https://download.redis.io/releases/redis-7.2.7.tar.gz -o "$root/redis.tar.gz"
 echo '72c081e3b8cfae7144273d26d76736f08319000af46c01515cad5d29765cead5  '"$root/redis.tar.gz" | sha256sum -c
 tar -xf "$root/redis.tar.gz" -C "$root"
fi
make -C "$root/redis-7.2.7" -j8 CC="$SEEDBOX_STATIC_CC" MALLOC=libc USE_SYSTEMD=no BUILD_TLS=no OPT=-O2 LDFLAGS=-static redis-server
mkdir -p "$root/guest-root"/{dev,proc,sys}
"$SEEDBOX_STATIC_CC" -O2 -static "$root/guest.c" -o "$root/guest-root/init"
cp "$root/redis-7.2.7/src/redis-server" "$root/guest-root/redis-server"
cd "$root/guest-root"
find . -print0 | sort -z | cpio --null -o --format=newc > "$root/inputs/redis-initramfs"
sha256sum "$root/inputs/clock-kernel" "$root/inputs/redis-initramfs" "$root/kernel-out/.config" "$root/guest-root/init" "$root/guest-root/redis-server"
