#!/usr/bin/env bash
# sweep.sh: 2 vCPU をテナント単位で k 分割したとき、テナント数 N（各 1 秒ごとにポーリング）でどこまで耐えるか。
#   構成: k=1 (共有 2000m) / k=2 (1000m×2) / k=4 (500m×4) / k=8 (250m×8)
#   テナントは k 個の DB に均等に割り当てる。負荷は N qps（= N テナント × 1 回/秒）。
source "$(dirname "$0")/lib.sh"
ROWS=${ROWS:-100}; SECS=${SECS:-12}
NMAX=${NMAX:-6000}
KS=${KS:-"1 2 4 8"}
NS=${NS:-"500 1000 2000 3000 4000 5000 6000"}
OUT=${OUT:-$ROOT/sweep.tsv}
echo -e "k\tcpu_each\tN\tachieved_qps\tworst_p50\tworst_p95\tworst_p99\tmax_ms\tdb_cpu_pct_total\tthrottled" > "$OUT"
for k in $KS; do
  millis=$((2000/k)); per=$((NMAX/k))
  for i in $(seq 0 $((k-1))); do pg_stop d$i; pg_start d$i $((5600+i)) $millis; seed $((5600+i)) $((i*per+1)) $(((i+1)*per)) $ROWS; done
  for N in $NS; do
    pids=(); : > $ROOT/res.$k.$N
    th0=0; for i in $(seq 0 $((k-1))); do th0=$((th0+$(throttled d$i))); done
    for i in $(seq 0 $((k-1))); do
      lo=$((i*per+1)); hi=$(( i*per + N/k )); [ $hi -gt $(((i+1)*per)) ] && hi=$(((i+1)*per))
      CLIENTS=$((64/k < 8 ? 8 : 64/k)) ./load.sh d$i $((5600+i)) $lo $hi $((N/k)) $SECS >> $ROOT/res.$k.$N &
      pids+=($!)
    done
    wait "${pids[@]}"
    th1=0; for i in $(seq 0 $((k-1))); do th1=$((th1+$(throttled d$i))); done
    python3 -I - $k $millis $N $ROOT/res.$k.$N $((th1-th0)) >> "$OUT" <<'PY'
import sys
k,ms,N,f,th=sys.argv[1:]
r=[l.split('\t') for l in open(f).read().strip().splitlines()]
v=lambda c:[float(x[c]) for x in r]
print(f"{k}\t{ms}m\t{N}\t{sum(v(1)):.0f}\t{max(v(2)):.2f}\t{max(v(3)):.2f}\t{max(v(4)):.2f}\t{max(v(5)):.1f}\t{sum(v(6)):.0f}\t{th}")
PY
    tail -1 "$OUT"
  done
  for i in $(seq 0 $((k-1))); do pg_stop d$i; done
done
