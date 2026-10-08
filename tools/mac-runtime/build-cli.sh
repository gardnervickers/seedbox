#!/usr/bin/env bash
# A single maintainer build produces the signed CLI and its preparation payload.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
source_revision=$(git rev-parse --verify HEAD)
source_dirty=0
if test -n "$(git --no-optional-locks status --porcelain --untracked-files=no)"; then source_dirty=1; fi
if test "$(uname -s)/$(uname -m)" != Darwin/arm64; then
  printf '%s\n' 'build-cli.sh requires an Apple Silicon Mac' >&2
  exit 2
fi
command -v limactl >/dev/null || { printf '%s\n' 'limactl not found; enter nix develop' >&2; exit 2; }
stage=$(mktemp -d /tmp/seedbox-mac-build.XXXXXX)
export LIMA_HOME="$stage/lima"
mkdir -m 700 "$LIMA_HOME"
cleanup() {
  limactl --tty=false stop --force seedbox-build >/dev/null 2>&1 || true
  limactl --tty=false delete --force seedbox-build >/dev/null 2>&1 || true
  rm -rf "$stage"
}
trap cleanup EXIT
printf '%s %s\n' "$source_revision" "$source_dirty" > "$stage/source-provenance"
limactl --tty=false start --name seedbox-build tools/mac-runtime/lima.yaml
COPYFILE_DISABLE=1 /usr/bin/tar --no-xattrs --no-mac-metadata --exclude=target --exclude=__pycache__ \
  -czf "$stage/source.tar.gz" Cargo.toml Cargo.lock flake.nix flake.lock .cargo crates guest kernel-arm64 tools/mac-runtime \
  examples/etcd/compose.yaml examples/etcd/workload/Dockerfile examples/etcd/workload/main.go \
  examples/etcd/workload/go.mod examples/etcd/workload/go.sum examples/etcd/workload/main_test.go \
  examples/redis/compose.yaml examples/redis/workload/Dockerfile examples/redis/workload/main.rs \
  -C "$stage" source-provenance
limactl --tty=false copy "$stage/source.tar.gz" seedbox-build:/tmp/seedbox-source.tar.gz
limactl --tty=false shell --workdir /tmp seedbox-build sh -c '
  set -eu
  mkdir /tmp/seedbox-source
  tar -xf /tmp/seedbox-source.tar.gz -C /tmp/seedbox-source
  cd /tmp/seedbox-source
  read -r SEEDBOX_SOURCE_REVISION SEEDBOX_SOURCE_DIRTY < source-provenance
  export SEEDBOX_SOURCE_REVISION SEEDBOX_SOURCE_DIRTY
  printf "%s\n" "target/" "**/target/" "__pycache__/" > .gitignore
  "$HOME/.nix-profile/bin/nix" develop --command sh -c "git init -q; git add ."
  "$HOME/.nix-profile/bin/nix" develop --command bash tools/mac-runtime/build-linux.sh /tmp/seedbox-package
'
mkdir -p artifacts/mac-runtime
limactl --tty=false copy seedbox-build:/tmp/seedbox-package/runtime.tar.gz artifacts/mac-runtime/runtime.tar.gz
SEEDBOX_MAC_RUNTIME_BUNDLE="$root/artifacts/mac-runtime/runtime.tar.gz" cargo build -p seedbox --release --locked
codesign --force --sign - --entitlements crates/seedbox-arm64/entitlements.plist target/release/seedbox
printf '%s\n' "built: $root/target/release/seedbox"
