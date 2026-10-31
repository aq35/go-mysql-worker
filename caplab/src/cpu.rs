//! DB の CPU 時間の採取（累積秒）。cgroup（推奨）か、同一ホストの DB プロセス（簡易）。
use std::fs;

#[derive(Clone, Debug)]
pub enum CpuSrc {
    /// cgroup v1 の cpuacct か v2 の cpu.stat。例: CAPLAB_CPU_CGROUP=tcl/s
    Cgroup(String),
    /// /proc から postgres / mysqld プロセスの合計（終了済み接続の CPU は含まれない簡易版）
    Proc,
    None,
}

impl CpuSrc {
    pub fn detect(engine_local: bool) -> CpuSrc {
        if let Ok(p) = std::env::var("CAPLAB_CPU_CGROUP") {
            if !p.is_empty() {
                return CpuSrc::Cgroup(p);
            }
        }
        if engine_local {
            return CpuSrc::Proc;
        }
        CpuSrc::None
    }
    pub fn name(&self) -> String {
        match self {
            CpuSrc::Cgroup(p) => format!("cgroup:{p}"),
            CpuSrc::Proc => "proc（簡易: 終了した接続の CPU を含まない）".into(),
            CpuSrc::None => "なし（リモート DB。CPU は CloudWatch 等から手で入力）".into(),
        }
    }
    /// 累積 CPU 秒
    pub fn read(&self) -> Option<f64> {
        match self {
            CpuSrc::Cgroup(p) => {
                if let Ok(s) =
                    fs::read_to_string(format!("/sys/fs/cgroup/cpuacct/{p}/cpuacct.usage"))
                {
                    return s.trim().parse::<f64>().ok().map(|n| n / 1e9);
                }
                let s = fs::read_to_string(format!("/sys/fs/cgroup/{p}/cpu.stat")).ok()?;
                s.lines()
                    .find_map(|l| l.strip_prefix("usage_usec "))
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .map(|u| u / 1e6)
            }
            CpuSrc::Proc => {
                let mut total = 0.0;
                for e in fs::read_dir("/proc").ok()?.flatten() {
                    let name = e.file_name();
                    let Some(pid) = name
                        .to_str()
                        .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
                    else {
                        continue;
                    };
                    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
                        continue;
                    };
                    let Some(open) = stat.find('(') else { continue };
                    let Some(close) = stat.rfind(')') else {
                        continue;
                    };
                    let comm = &stat[open + 1..close];
                    if comm != "postgres" && comm != "mysqld" && !comm.starts_with("postgres") {
                        continue;
                    }
                    let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
                    if f.len() > 13 {
                        let ut: f64 = f[11].parse().unwrap_or(0.0);
                        let st: f64 = f[12].parse().unwrap_or(0.0);
                        total += (ut + st) / 100.0;
                    }
                }
                Some(total)
            }
            CpuSrc::None => None,
        }
    }
}

/// このプロセス（負荷生成側）の累積 CPU 秒
pub fn self_cpu() -> f64 {
    let Ok(stat) = fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(close) = stat.rfind(')') else {
        return 0.0;
    };
    let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
    if f.len() > 13 {
        (f[11].parse::<f64>().unwrap_or(0.0) + f[12].parse::<f64>().unwrap_or(0.0)) / 100.0
    } else {
        0.0
    }
}
