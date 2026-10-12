#!/bin/sh
# Runs every case of one batch in sequence and frames each case's combined output.
# Usage: run-batch.sh BATCH   (cases come from /fuzz/batches/BATCH.tsv)
# Each TSV line is: id<TAB>timeout-seconds<TAB>jvm flags<TAB>main class<TAB>program arguments
batch="$1"
cd /tmp || exit 2
tab=$(printf '\t')
while IFS="$tab" read -r id limit flags main args; do
    echo "@@CASE $id"
    # shellcheck disable=SC2086
    # Each case gets its own temporary directory, removed afterwards, so files a
    # crashed JVM leaves (a JFR repository, a heap dump) cannot fill the disk.
    mkdir -p /tmp/fzcase
    timeout -s KILL "$limit" java $flags -Djava.io.tmpdir=/tmp/fzcase -XX:ErrorFile=/tmp/hs_err_$id.log \
        -cp /fuzz/classes "$main" $args < /dev/null 2>&1
    code=$?
    rm -rf /tmp/fzcase /tmp/.java_pid* /tmp/.attach_pid*
    if [ -f "/tmp/hs_err_$id.log" ]; then
        echo "@@HSERR $id"
        head -c 24000 "/tmp/hs_err_$id.log" | sed 's/^/@@| /'
        echo
        rm -f "/tmp/hs_err_$id.log"
    fi
    echo "@@EXIT $id $code"
done < "/fuzz/batches/$batch.tsv"
echo "@@DONE $batch"
