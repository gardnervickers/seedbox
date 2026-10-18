# Sourced by the official entrypoint after initdb: let standbys stream from
# this server with trust authentication, as POSTGRES_HOST_AUTH_METHOD=trust
# does for ordinary connections ("all" does not match replication).
echo "host replication all all trust" >> "$PGDATA/pg_hba.conf"
