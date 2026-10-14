"""Diagnostic machine: idle until --at seconds, then report what pg's backends wait on."""
import json, sys, time
sys.path.insert(0, "/scenario")
from pg_append import Pg, PgError, Indeterminate

QUERIES = {
    "activity": "SELECT pid, state, wait_event_type, wait_event, backend_xid, backend_xmin, "
                "extract(epoch FROM now() - xact_start)::float8, extract(epoch FROM now() - state_change)::float8, "
                "pg_blocking_pids(pid), left(query, 70) FROM pg_stat_activity WHERE backend_type = 'client backend' ORDER BY pid",
    "waiting": "SELECT pid, locktype, mode, relation::regclass, page, tuple, transactionid, virtualtransaction "
               "FROM pg_locks WHERE NOT granted ORDER BY pid",
    "holders": "SELECT l.pid, l.locktype, l.mode, l.transactionid, l.relation::regclass, a.state "
               "FROM pg_locks l LEFT JOIN pg_stat_activity a USING (pid) WHERE l.granted AND l.locktype IN ('transactionid','tuple') ORDER BY l.pid",
    "settings": "SELECT name, setting FROM pg_settings WHERE name IN ('deadlock_timeout','max_connections','lock_timeout')",
}
times = [float(x) for x in sys.argv[1:]] or [14, 18]
start = time.monotonic()
for at in times:
    time.sleep(max(0, at - (time.monotonic() - start)))
    try:
        pg = Pg("pg", 5432, 5)
        out = {"t": round(time.monotonic() - start, 3)}
        for name, sql in QUERIES.items():
            out[name] = pg.query(sql)[0]
        pg.close()
        print("PROBE " + json.dumps(out), flush=True)
    except (OSError, PgError, Indeterminate) as error:
        print("PROBE-ERROR " + str(error), flush=True)
time.sleep(3600)
