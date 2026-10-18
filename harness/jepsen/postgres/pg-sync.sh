# Sourced by the official entrypoint after initdb and its own pg_hba.conf
# edit: flush them. initdb syncs the cluster, but the entrypoint then appends
# "host all all all <method>" to pg_hba.conf without a sync, so a power
# failure in the next ~30 s (before writeback) loses that line and every
# client is refused ("no pg_hba.conf entry") from then on.
sync
