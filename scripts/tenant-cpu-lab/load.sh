#!/usr/bin/env bash
# load.sh <name> <port> <lo> <hi> <rate_qps> <secs> [sql]
#   テナント lo..hi をランダムに選んで rate qps で引く。出力: name tps p50 p95 p99 max cpu%（ms）
source "$(dirname "$0")/lib.sh"
name=$1 port=$2 lo=$3 hi=$4 rate=$5 secs=$6 sql=${7:-$(dirname "$0")/poll.sql}
clients=${CLIENTS:-32}
log=$ROOT/pgblog.$name; rm -f $log*
c0=$(cpu_secs $name)
taskset -c $LOAD_CORES pgbench -n -h 127.0.0.1 -p $port -U postgres lab -f "$sql" \
  -D lo=$lo -D hi=$hi -c $clients -j 2 -R $rate -T $secs -l --log-prefix=$log >$ROOT/pgb.$name.out 2>&1 || true
c1=$(cpu_secs $name)
python3 -I - "$log" "$ROOT/pgb.$name.out" "$name" "$secs" "$c0" "$c1" <<'PY'
import sys,glob
log,out,name,secs,c0,c1=sys.argv[1:]
lat=[]
for f in glob.glob(log+"*"):
    for l in open(f):
        p=l.split()
        if len(p)>2: lat.append(int(p[2])/1000)
lat.sort()
n=len(lat)
q=lambda x: lat[min(n-1,int(n*x))] if n else float('nan')
cpu=(float(c1)-float(c0))/float(secs)*100
print(f"{name}\t{n/float(secs):.0f}\t{q(.5):.2f}\t{q(.95):.2f}\t{q(.99):.2f}\t{lat[-1] if n else 0:.1f}\t{cpu:.0f}")
PY
