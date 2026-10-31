#!/usr/bin/env bash
# テナント単位 CPU 分離実験の共通関数（PostgreSQL 16 / cgroup v1 cpu quota）
#   DB は cgroup の cfs_quota で「N vCPU」に制限する。負荷側 (pgbench) は別コアに固定する。
set -euo pipefail
PGBIN=${PGBIN:-/usr/lib/postgresql/16/bin}
ROOT=${LAB_ROOT:-/tmp/tcl}
CG=/sys/fs/cgroup/cpu/tcl
CA=/sys/fs/cgroup/cpuacct/tcl
DB_CORES=${DB_CORES:-0,1}      # DB を載せるコア（= 2 vCPU の実体）
LOAD_CORES=${LOAD_CORES:-2,3}  # 負荷生成側

pg_as() { su postgres -s /bin/bash -c "$*"; }

# pg_start <name> <port> <cpu_millis>   例: pg_start t0 5500 1000  → 1 vCPU
pg_start() {
  local name=$1 port=$2 millis=$3; local dir=$ROOT/$name
  mkdir -p "$ROOT"; chown postgres "$ROOT"
  rm -rf "$dir"
  pg_as "$PGBIN/initdb -D $dir -A trust >/dev/null"
  cat >> "$dir/postgresql.conf" <<CONF
port=$port
listen_addresses='127.0.0.1'
unix_socket_directories='$ROOT'
max_connections=200
shared_buffers=256MB
fsync=off
synchronous_commit=off
autovacuum=off
log_min_messages=fatal
CONF
  mkdir -p "$CG/$name" "$CA/$name"
  echo $((millis*100)) > "$CG/$name/cpu.cfs_quota_us"   # period=100000us
  # 起動した postmaster ごと cgroup に入れてから子プロセスを増やす
  pg_as "taskset -c $DB_CORES $PGBIN/pg_ctl -D $dir -l $ROOT/$name.log -w start >/dev/null"
  local pm; pm=$(head -1 "$dir/postmaster.pid")
  echo "$pm" > "$CG/$name/cgroup.procs"; echo "$pm" > "$CA/$name/cgroup.procs"
  for c in $(pgrep -P "$pm"); do echo "$c" > "$CG/$name/cgroup.procs" 2>/dev/null || true; echo "$c" > "$CA/$name/cgroup.procs" 2>/dev/null || true; done
  # 以後に増える backend は親 (postmaster) の cgroup を継承する
}

pg_stop() { pg_as "$PGBIN/pg_ctl -D $ROOT/$1 -m immediate -w stop >/dev/null 2>&1" || true; rmdir "$CG/$1" "$CA/$1" 2>/dev/null || true; }

psqlp() { local port=$1; shift; psql -h 127.0.0.1 -p "$port" -U postgres -d lab -qAt "$@"; }

# DB が使った CPU 秒（cpuacct.usage = ns。終了済み backend の分も含む）
cpu_secs() { echo "scale=3; $(cat "$CA/$1/cpuacct.usage")/1000000000" | bc; }
throttled() { awk '/nr_throttled/{print $2}' "$CG/$1/cpu.stat"; }

# seed <port> <tenant_from> <tenant_to> <rows>   a=rows行×6列, b=rows行×3列
seed() {
  local port=$1 from=$2 to=$3 rows=$4
  psql -h 127.0.0.1 -p "$port" -U postgres -d postgres -qAt -c "CREATE DATABASE lab" 2>/dev/null || true
  psqlp "$port" <<SQL
CREATE TABLE IF NOT EXISTS a(tenant_id int, id int, c1 text, c2 text, c3 int, c4 int, PRIMARY KEY(tenant_id,id));
CREATE TABLE IF NOT EXISTS b(tenant_id int, a_id int, v int, PRIMARY KEY(tenant_id,a_id));
INSERT INTO a SELECT t,i,'name'||i,'status'||(i%5),i%7,i FROM generate_series($from,$to) t, generate_series(1,$rows) i ON CONFLICT DO NOTHING;
INSERT INTO b SELECT t,i,i%100 FROM generate_series($from,$to) t, generate_series(1,$rows) i ON CONFLICT DO NOTHING;
ANALYZE;
SQL
}

# my_start <name> <port> <cpu_millis>  — MySQL 8.0 を cgroup の CPU 上限つきで起動する（root 不要の mysql ユーザで実行）
my_start() {
  local name=$1 port=$2 millis=$3; local dir=$ROOT/$name
  mkdir -p "$ROOT"; chown mysql:mysql "$ROOT" 2>/dev/null || true
  rm -rf "$dir"; mkdir -p "$dir"; chown mysql:mysql "$dir"
  su mysql -s /bin/bash -c "mysqld --no-defaults --initialize-insecure --datadir=$dir >$ROOT/$name.init.log 2>&1"
  mkdir -p "$CG/$name" "$CA/$name"; echo $((millis*100)) > "$CG/$name/cpu.cfs_quota_us"
  su mysql -s /bin/bash -c "taskset -c $DB_CORES mysqld --no-defaults --datadir=$dir --port=$port --bind-address=127.0.0.1 --socket=$ROOT/$name.sock --mysqlx=OFF --max-connections=300 --innodb-buffer-pool-size=256M --innodb-flush-log-at-trx-commit=1 --skip-log-bin --pid-file=$ROOT/$name.pid --log-error=$ROOT/$name.err >/dev/null 2>&1 &"
  for _ in $(seq 1 60); do mysqladmin -h127.0.0.1 -P$port -uroot ping >/dev/null 2>&1 && break; sleep 1; done
  local pm; pm=$(cat "$ROOT/$name.pid")
  echo "$pm" > "$CG/$name/cgroup.procs"; echo "$pm" > "$CA/$name/cgroup.procs"
  for t in $(ls /proc/$pm/task); do echo "$t" > "$CG/$name/cgroup.procs" 2>/dev/null || true; echo "$t" > "$CA/$name/cgroup.procs" 2>/dev/null || true; done
  mysql -h127.0.0.1 -P$port -uroot -e "CREATE DATABASE IF NOT EXISTS caplab_lab; CREATE USER IF NOT EXISTS 'cap'@'%' IDENTIFIED WITH mysql_native_password BY 'cap'; GRANT ALL ON *.* TO 'cap'@'%'; FLUSH PRIVILEGES;"
}
my_stop() { mysqladmin -h127.0.0.1 -P$2 -uroot shutdown 2>/dev/null || true; sleep 1; rmdir "$CG/$1" "$CA/$1" 2>/dev/null || true; }
