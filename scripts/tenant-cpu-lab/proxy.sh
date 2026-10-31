#!/usr/bin/env bash
# proxy.sh: アプリ → Proxy(PgBouncer) → PostgreSQL の 3 層で接続数を同時に見る。
#   RDS Proxy 相当のローカル代用品。ピン留めの規則は RDS Proxy と同じではない（概念の確認用）。
source "$(dirname "$0")/lib.sh"
PGPORT=5900; BPORT=6432; SECS=${SECS:-12}; RATE=${RATE:-500}
OUT=${OUT:-$ROOT/proxy.tsv}
pg_stop px; pg_start px $PGPORT 2000; seed $PGPORT 1 100 100

start_bouncer() { # mode pool
  pg_as "pkill -u postgres pgbouncer" 2>/dev/null || true; sleep 0.5
  cat > $ROOT/pgbouncer.ini <<CONF
[databases]
lab = host=127.0.0.1 port=$PGPORT dbname=lab
[pgbouncer]
listen_addr = 127.0.0.1
listen_port = $BPORT
auth_type = trust
auth_file = $ROOT/userlist.txt
admin_users = postgres
pool_mode = $1
default_pool_size = $2
max_client_conn = 2000
ignore_startup_parameters = extra_float_digits
logfile = $ROOT/pgbouncer.log
pidfile = $ROOT/pgbouncer.pid
CONF
  echo '"postgres" ""' > $ROOT/userlist.txt; chown postgres $ROOT/pgbouncer.ini $ROOT/userlist.txt
  ulimit -n 8192; pg_as "ulimit -n 8192; pgbouncer -d $ROOT/pgbouncer.ini"; sleep 1
}

# 1 秒ごとに 3 層を採取して最大値を出す
sampler() { # outfile
  set +e +o pipefail
  : > $1
  while [ -e $ROOT/sampling ]; do
    cl=$(psql -h 127.0.0.1 -p $BPORT -U postgres pgbouncer -qAt -F, -c "SHOW LISTS" 2>/dev/null | awk -F, '$1=="used_clients"{print $2}')
    pools=$(psql -h 127.0.0.1 -p $BPORT -U postgres pgbouncer -qAt -F, -c "SHOW POOLS" 2>/dev/null | awk -F, '$1=="lab"{sa+=$7; w+=$4; sv+=$7+$10+$11} END{print sa+0","w+0","sv+0}')
    pg=$(psqlp $PGPORT -F, -c "select count(*) filter (where backend_type='client backend' and datname='lab'), count(*) filter (where state='active' and backend_type='client backend' and datname='lab' and pid<>pg_backend_pid()) from pg_stat_activity")
    echo "${cl:-0},$pools,$pg" >> $1; sleep 1
  done
}

# run <label> <via: direct|bouncer> <clients> <sqlfile>
run() {
  local label=$1 via=$2 c=$3 sql=$4 port; port=$([ $via = direct ] && echo $PGPORT || echo $BPORT)
  touch $ROOT/sampling; sampler $ROOT/samp.csv & local sp=$!
  local r; r=$(CLIENTS=$c ./load.sh px $port 1 100 $RATE $SECS $sql)
  rm -f $ROOT/sampling; wait $sp 2>/dev/null || true
  python3 -I - "$label" "$via" "$c" "$r" "$ROOT/samp.csv" >> "$OUT" <<'PY'
import sys
label,via,c,r,f=sys.argv[1:]
rows=[list(map(int,l.strip().split(','))) for l in open(f) if l.strip().count(',')==5]
rows=rows[2:] or rows            # 立ち上がりを除く
mx=lambda i:max((x[i] for x in rows),default=0)
t=r.split('\t')[1:]             # tps p50 p95 p99 max cpu
client = int(c) if via=='direct' else mx(0)   # アプリ側の接続数（直結は pgbench の -c）
print("\t".join([label,via,c,str(client),str(mx(1)),str(mx(2)),str(mx(3)),str(mx(4)),str(mx(5))]+t[:1]+t[2:3]+t[3:4]+t[5:6]))
PY
  tail -1 "$OUT"
}
echo -e "case\tvia\tapp_clients\tproxy_clients_seen\tproxy_sv_active\tproxy_cl_waiting\tproxy_sv_total\tdb_conns\tdb_active\tqps\tp95\tp99\tdb_cpu%" > "$OUT"
P=poll.sql; H=hold.sql
run "直結 100"                 direct 100 $P
start_bouncer transaction 10
run "Proxy(txn,pool10) 100"    bouncer 100 $P
run "Proxy(txn,pool10) 400"    bouncer 400 $P
run "Proxy(txn,pool10) 100 + tx内50ms待ち" bouncer 100 $H
start_bouncer transaction 40
run "Proxy(txn,pool40) 100 + tx内50ms待ち" bouncer 100 $H
start_bouncer session 400
run "Proxy(session=全部ピン留め相当) 100" bouncer 100 $P
pg_as "pkill -u postgres pgbouncer" || true; pg_stop px
