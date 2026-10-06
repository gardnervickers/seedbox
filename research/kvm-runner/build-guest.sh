#!/usr/bin/env bash
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
SEEDBOX_STATIC_CC="${SEEDBOX_STATIC_CC%/cc}/x86_64-unknown-linux-musl-cc"
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
