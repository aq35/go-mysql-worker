source "$(dirname "$0")/lib.sh"
pg_stop s; pg_start s 5500 2000
seed 5500 1 100 100
./load.sh s 5500 1 100 100 5
cat /sys/fs/cgroup/cpu/tcl/s/cgroup.procs | wc -l
