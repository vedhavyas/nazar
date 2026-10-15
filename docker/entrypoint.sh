#!/bin/sh
# Runs as root only long enough to make the data volume writable, then
# drops to the image's unprivileged uid.
set -eu

data="${DATA_DIR:-/data}"
mkdir -p "$data"
chown -R 101:101 "$data"

exec setpriv --reuid=101 --regid=101 --clear-groups /usr/local/bin/nazar-server
