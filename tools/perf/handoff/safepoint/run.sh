#!/bin/sh
# usage: run.sh BUNDLE|native [--dr "EXTRA DR OPTIONS"] [--noclient] -- REPRO_ARGS...
# Runs out/repro in a local linux/arm64 alpine container. BUNDLE names a directory
# under bundles/ that is mounted at /seedbox/progress (drrun's fixed interpreter path).
# DR is launched exactly as guest/compose-init.c launches applications.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bundle=$1; shift
extra=""; client=progress; budget=50000
while [ "$1" != "--" ]; do
    case $1 in
        --dr) extra=$2; shift 2 ;;
        --noclient) client=none; shift ;;
        --park) client=park; shift ;;
        --park309) client=park309; shift ;;
        --both) client=both; shift ;;
        --budget) budget=$2; shift 2 ;;
        *) echo "bad arg $1" >&2; exit 2 ;;
    esac
done
shift
prefix=""
mounts="-v $here/out:/r:ro -v $here/logs:/logs"
if [ "$bundle" = native ]; then
    cmd="$prefix /r/repro $*"
else
    mounts="$mounts -v $here/bundles/$bundle:/seedbox/progress:ro"
    case $client in
        progress) c="-c /seedbox/progress/libseedbox_progress.so --instructions-per-progress-call $budget --progress-points-per-block 1" ;;
        park) c="-c /r/librepro_park.so" ;;
        park309) c="-c /r/librepro_park.so redirect" ;;
        # drrun's -c can name only one client and treats everything up to -- as
        # its args, so two clients go through the DR -client_lib64 list option
        # (path;id;options triples, ';'-joined by the core option parser).
        both) c="-client_lib64 '/seedbox/progress/libseedbox_progress.so;0;--instructions-per-progress-call $budget --progress-points-per-block 1' -client_lib64 '/r/librepro_park.so;1;'" ;;
        none) c="" ;;
    esac
    cmd="$prefix /seedbox/progress/bin64/drrun -opt_speed -disable_traces -ldstex2cas $extra $c -- /r/repro $*"
fi
exec podman run --rm --platform linux/arm64 --security-opt seccomp=unconfined $mounts \
    docker.io/library/alpine:3.22 sh -c "cd /logs && $cmd"
