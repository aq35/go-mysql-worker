//! ネットワークの演出（遅延を挟む TCP 中間サーバ）と、RTT の計測（probe）。
use crate::engine::Engine;
use crate::model::EngineKind;
use anyhow::{anyhow, Result};
use rand::Rng;
use serde::Serialize;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// tokio のタイマーは約 1ms 単位なので、短い待ちは最後だけ yield で空回りして精度を出す（負荷側の CPU を使う）
async fn precise_sleep_until(at: Instant) {
    loop {
        let now = Instant::now();
        if now >= at {
            return;
        }
        let rem = at - now;
        if rem > Duration::from_micros(2500) {
            tokio::time::sleep(rem - Duration::from_micros(2000)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}

async fn pipe(mut r: OwnedReadHalf, mut w: OwnedWriteHalf, one_way: Duration, jitter: Duration) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((at, data)) = rx.recv().await {
            precise_sleep_until(at).await;
            if w.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    let mut buf = vec![0u8; 65536];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let j = if jitter.is_zero() { Duration::ZERO } else { Duration::from_secs_f64(rand::thread_rng().gen::<f64>() * jitter.as_secs_f64()) };
                // 順序は保つ（書き出し側は 1 本で順に処理する）
                let _ = tx.send((Instant::now() + one_way + j, buf[..n].to_vec()));
            }
        }
    }
    drop(tx);
    let _ = writer.await;
}

pub struct NetSim {
    pub addr: SocketAddr,
    handle: JoinHandle<()>,
}
impl NetSim {
    /// `target` は "host:port"。片方向の遅延 `one_way_ms` とジッターを挟んで中継する。接続の確立にも 1 RTT 分の遅延を入れる。
    pub async fn start(target: String, one_way_ms: f64, jitter_ms: f64) -> Result<NetSim> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let (ow, jt) = (Duration::from_secs_f64(one_way_ms.max(0.0) / 1000.0), Duration::from_secs_f64(jitter_ms.max(0.0) / 1000.0));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((c, _)) = l.accept().await else { break };
                let target = target.clone();
                tokio::spawn(async move {
                    precise_sleep_until(Instant::now() + ow * 2).await;
                    let Ok(s) = TcpStream::connect(&target).await else { return };
                    let _ = (c.set_nodelay(true), s.set_nodelay(true));
                    let (cr, cw) = c.into_split();
                    let (sr, sw) = s.into_split();
                    tokio::join!(pipe(cr, sw, ow, jt), pipe(sr, cw, ow, jt));
                });
            }
        });
        Ok(NetSim { addr, handle })
    }
}
impl Drop for NetSim {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// DSN から接続先 (host, port) を取り出す
pub fn host_port(kind: EngineKind, dsn: &str) -> (String, u16) {
    match kind {
        EngineKind::Mysql => {
            let rest = dsn.split("://").nth(1).unwrap_or("");
            let hp = rest.rsplit('@').next().unwrap_or("").split('/').next().unwrap_or("");
            let mut it = hp.split(':');
            (it.next().unwrap_or("127.0.0.1").to_string(), it.next().and_then(|p| p.parse().ok()).unwrap_or(3306))
        }
        EngineKind::Postgres => {
            let get = |k: &str| dsn.split_whitespace().find_map(|t| t.strip_prefix(k)).map(|s| s.to_string());
            (get("host=").unwrap_or_else(|| "127.0.0.1".into()), get("port=").and_then(|p| p.parse().ok()).unwrap_or(5432))
        }
    }
}
/// DSN の接続先を差し替える
pub fn rewrite_dsn(kind: EngineKind, dsn: &str, addr: SocketAddr) -> String {
    match kind {
        EngineKind::Mysql => {
            let (scheme, rest) = dsn.split_once("://").unwrap_or(("mysql", dsn));
            let (auth, after) = match rest.rsplit_once('@') {
                Some((a, b)) => (format!("{a}@"), b.to_string()),
                None => (String::new(), rest.to_string()),
            };
            let tail = after.find('/').map(|i| after[i..].to_string()).unwrap_or_default();
            format!("{scheme}://{auth}{}:{}{tail}", addr.ip(), addr.port())
        }
        EngineKind::Postgres => {
            let mut toks: Vec<String> = dsn.split_whitespace().filter(|t| !t.starts_with("host=") && !t.starts_with("port=")).map(|s| s.to_string()).collect();
            toks.push(format!("host={}", addr.ip()));
            toks.push(format!("port={}", addr.port()));
            toks.join(" ")
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ProbeResult {
    pub connect_ms: f64,
    pub rtt_p50_ms: f64,
    pub rtt_p95_ms: f64,
    pub rtt_min_ms: f64,
    pub samples: usize,
}
fn pct(v: &[f64], q: f64) -> f64 {
    if v.is_empty() { 0.0 } else { v[((v.len() as f64 - 1.0) * q).round() as usize] }
}

/// 接続の確立時間と、1 接続で `SELECT 1` を n 回実行した往復時間を測る
pub async fn probe(engine: &Engine, n: usize) -> Result<ProbeResult> {
    probe_gap(engine, n, 0).await
}

/// `gap_ms` ミリ秒の間隔を空けながら測る（アイドルの後にサーバの接続が起こされる遅さを見る）
pub async fn probe_gap(engine: &Engine, n: usize, gap_ms: u64) -> Result<ProbeResult> {
    let mut conns = vec![];
    for _ in 0..5 {
        let t = Instant::now();
        let c = engine.connect().await.map_err(|e| anyhow!("probe: {e}"))?;
        conns.push((t.elapsed().as_secs_f64() * 1000.0, c));
    }
    let mut cs: Vec<f64> = conns.iter().map(|x| x.0).collect();
    cs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (_, mut c) = conns.remove(0);
    let _ = c.exec("SELECT 1").await?; // ウォームアップ
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        if gap_ms > 0 {
            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
        }
        let t = Instant::now();
        c.exec("SELECT 1").await?;
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(ProbeResult { connect_ms: pct(&cs, 0.5), rtt_p50_ms: pct(&v, 0.5), rtt_p95_ms: pct(&v, 0.95), rtt_min_ms: v.first().copied().unwrap_or(0.0), samples: v.len() })
}
