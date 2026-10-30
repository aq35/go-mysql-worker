#!/usr/bin/env bash
# noisy.sh: 通常 N テナントのポーリング中に、重い 1 テナントがバッチを流したとき、誰が巻き込まれるか。
#   重いテナントは tenant 1..50 を持つ DB（d0）に載る。
source "$(dirname "$0")/lib.sh"
ROWS=${ROWS:-100}; SECS=${SECS:-15}; NMAX=${NMAX:-6000}; N=${N:-1500}
KS=${KS:-"1 2 4 8"}; HEAVY=${HEAVY:-1}
OUT=${OUT:-$ROOT/noisy.tsv}
echo -e "k\tcpu_each\theavy\tgroup\tqps\tp50\tp95\tp99\tmax_ms\tcpu_pct" > "$OUT"
for k in $KS; do
  millis=$((2000/k)); per=$((NMAX/k))
  for i in $(seq 0 $((k-1))); do pg_stop d$i; pg_start d$i $((5600+i)) $millis; seed $((5600+i)) $((i*per+1)) $(((i+1)*per)) $ROWS; done
  for h in 0 $HEAVY; do
    pids=(); rm -f $ROOT/nres.* $ROOT/heavy.out
    for i in $(seq 0 $((k-1))); do
      lo=$((i*per+1)); hi=$((i*per + N/k))
      CLIENTS=$((64/k < 8 ? 8 : 64/k)) ./load.sh d$i $((5600+i)) $lo $hi $((N/k)) $SECS > $ROOT/nres.$i &
      pids+=($!)
    done
    if [ "$h" = 1 ]; then
      taskset -c $LOAD_CORES pgbench -n -h 127.0.0.1 -p 5600 -U postgres lab -f heavy.sql -c 2 -j 1 -T $((SECS+1)) > $ROOT/heavy.out 2>&1 &
      pids+=($!)
    fi
    wait "${pids[@]}"
    python3 -I - $k $millis $h $ROOT $OUT $(awk "/tps =/{print \$3}" $ROOT/heavy.out 2>/dev/null | head -1 | grep . || echo -) <<'PY'
import sys,glob
k,ms,h,root,out,htps=sys.argv[1:7]
rows={int(f.rsplit('.',1)[1]):open(f).read().split('\t') for f in glob.glob(root+'/nres.*')}
def line(g,r): return f"{k}\t{ms}m\t{h}\t{g}\t{float(r[1]):.0f}\t{r[2]}\t{r[3]}\t{r[4]}\t{r[5]}\t{float(r[6]):.0f}\n"
with open(out,'a') as o:
    o.write(line('d0(重いテナントと同居)',rows[0]))
    if len(rows)>1:
        w=max((rows[i] for i in rows if i>0), key=lambda r: float(r[3]))
        o.write(line('他のDB(最悪)',w))
    if h=='1': o.write(f"{k}\t{ms}m\t{h}\t重いバッチ\t{htps}/s\t\t\t\t\t\n")
PY
  done
  tail -4 "$OUT"
  for i in $(seq 0 $((k-1))); do pg_stop d$i; done
done
