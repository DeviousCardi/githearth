#!/bin/sh
set -eu
pids=''
trap 'kill $pids 2>/dev/null || true; wait || true' TERM INT
for port in 14500 14501 14502 14503; do
    mkdir -p "/var/lib/foundationdb/data/$port"
    /usr/bin/fdbserver -p "127.0.0.1:$port" -C /config/fdb.cluster \
        -d "/var/lib/foundationdb/data/$port" -L /tmp --maxlogssize 10MiB \
        --memory 700MiB --cache-memory 128MiB -i 0000000000000001 &
    pids="$pids $!"
done
wait
