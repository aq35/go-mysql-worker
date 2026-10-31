# テナント単位の CPU 分離実験

2 vCPU の DB を、テナントをグループ分けして **1 / 2 / 4 / 8 個の DB に分割**（各 2000m / 1000m / 500m / 250m）したとき、
何テナント（各 1 秒ごとにポーリング）まで耐えるかを測る。結果と考察は [docs/tenant-cpu-isolation.md](../../docs/tenant-cpu-isolation.md)。

要件: PostgreSQL 16 サーバ、pgbench、root（cgroup v1 の `cpu` / `cpuacct`）、taskset、python3。Docker は不要。

```bash
cd scripts/tenant-cpu-lab
OUT=/tmp/tcl/sweep.tsv ./sweep.sh                      # テナント数を 500〜6000 で掃引
NS="2200 2400 2600 2800" OUT=/tmp/tcl/knee.tsv ./sweep.sh   # 限界付近を細かく
OUT=/tmp/tcl/noisy.tsv ./noisy.sh                      # 重い 1 テナントを混ぜる
```

DB は CPU コア 0,1 に固定し、各 DB に cgroup の `cfs_quota` で CPU 上限を与える。負荷側（pgbench）はコア 2,3。
