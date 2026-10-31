#!/usr/bin/env bash
# net.sh: ネットワーク遅延(RTT)を変えて、1 ループ(直列 12 クエリ)の所要・スループット・DB の CPU を測る。
source "$(dirname "$0")/lib.sh"
OUT=${OUT:-$ROOT/net.tsv}; SECS=${SECS:-10}; C=${C:-16}
pg_stop nt; pg_start nt 5900 2000; seed 5900 1 100 100
echo -e "rtt_ms\tclients\tloops_per_s\tloop_ms_avg\tqueries_per_s\tdb_cpu_pct" > "$OUT"
for rtt in 0 0.05 0.5 1 2 5; do
  if [ "$rtt" = 0 ]; then port=5900; else
    half=$(echo "scale=4; $rtt/2" | bc); python3 delayproxy.py 6000 5900 $half & dp=$!; sleep 0.5; port=6000
  fi
  c0=$(cpu_secs nt)
  res=$(taskset -c $LOAD_CORES pgbench -n -h 127.0.0.1 -p $port -U postgres lab -f serial12.sql -c $C -j 2 -T $SECS 2>&1)
  c1=$(cpu_secs nt)
  tps=$(echo "$res" | awk '/^tps =/{print $3}'); lat=$(echo "$res" | awk '/latency average/{print $4}')
  python3 -I -c "
tps=float('$tps'); lat=float('$lat'); cpu=(float('$c1')-float('$c0'))/$SECS*100
print('\t'.join(['$rtt','$C',f'{tps:.0f}',f'{lat:.1f}',f'{tps*12:.0f}',f'{cpu:.0f}']))" >> "$OUT"
  tail -1 "$OUT"
  [ "$rtt" != 0 ] && { kill $dp 2>/dev/null; wait $dp 2>/dev/null; } || true
done
pg_stop nt
