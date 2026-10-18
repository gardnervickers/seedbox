#!/bin/bash
# A streaming standby of $PRIMARY. On first boot, copy the primary with
# pg_basebackup (-R writes standby.signal and primary_conninfo), retrying until
# the primary accepts replication connections; then run postgres through
# pgmon.sh, whose entrypoint finds the copied cluster and skips initdb.
: "${PGDATA:=/var/lib/postgresql/18/docker}"
if [ ! -s "$PGDATA/PG_VERSION" ]; then
  mkdir -p "$PGDATA"
  chown -R postgres:postgres /var/lib/postgresql
  chmod 700 "$PGDATA"
  until gosu postgres pg_basebackup -h "$PRIMARY" -U postgres -D "$PGDATA" -R -X stream -c fast; do
    echo "PGMON basebackup from $PRIMARY failed; retrying" >&2
    find "$PGDATA" -mindepth 1 -delete
    sleep 1
  done
  echo "PGMON basebackup from $PRIMARY done" >&2
fi
exec bash /pgmon.sh "$@"
