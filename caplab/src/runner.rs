//! 負荷実行: 開ループのスケジューラ、直列ステップ（親→子→孫）、接続プール、メトリクス採取。
use crate::cpu::{self, CpuSrc};
use crate::engine::{Conn, Engine};
use crate::model::*;
use crate::schema;
use hdrhistogram::Histogram;
use rand::Rng;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};

fn hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_max(120_000_000, 3).unwrap()
}
fn rec(h: &mut Histogram<u64>, d: Duration) {
    let _ = h.record((d.as_micros() as u64).clamp(1, 120_000_000));
}
fn ms(h: &Histogram<u64>, q: f64) -> f64 {
    if h.is_empty() {
        0.0
    } else {
        h.value_at_quantile(q) as f64 / 1000.0
    }
}

// ---------------- 停止の合図 ----------------
pub struct Stop {
    flag: AtomicBool,
    n: tokio::sync::Notify,
}
impl Stop {
    pub fn new() -> Arc<Stop> {
        Arc::new(Stop {
            flag: AtomicBool::new(false),
            n: tokio::sync::Notify::new(),
        })
    }
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
    pub fn set(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.n.notify_waiters();
    }
    pub async fn wait(&self) {
        loop {
            let f = self.n.notified();
            tokio::pin!(f);
            f.as_mut().enable();
            if self.is_set() {
                return;
            }
            f.await;
        }
    }
}
/// 次の実行時刻まで眠る。停止が来たら起きる。
async fn nap_until(stop: &Stop, at: Instant) {
    tokio::select! {
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {}
        _ = stop.wait() => {}
    }
}

// ---------------- 接続プール ----------------
pub struct Pool {
    engine: Engine,
    idle: Mutex<Vec<Box<dyn Conn>>>,
    sem: Arc<Semaphore>,
    pub in_use: AtomicI64,
    pub waiting: AtomicI64,
    pub wait_total_us: AtomicU64,
    pub wait_count: AtomicU64,
    pub opened: AtomicU64,
}
pub struct Guard {
    conn: Option<Box<dyn Conn>>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
    pub broken: bool,
}
impl Pool {
    pub fn new(engine: Engine, max_open: u32) -> Arc<Pool> {
        Arc::new(Pool {
            engine,
            idle: Mutex::new(vec![]),
            sem: Arc::new(Semaphore::new(max_open.max(1) as usize)),
            in_use: 0.into(),
            waiting: 0.into(),
            wait_total_us: 0.into(),
            wait_count: 0.into(),
            opened: 0.into(),
        })
    }
    pub async fn get(self: &Arc<Pool>) -> anyhow::Result<Guard> {
        self.waiting.fetch_add(1, Ordering::Relaxed);
        let t = Instant::now();
        let permit = self.sem.clone().acquire_owned().await.unwrap();
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        self.wait_total_us
            .fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.wait_count.fetch_add(1, Ordering::Relaxed);
        let idle = self.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(c) => c,
            None => {
                self.opened.fetch_add(1, Ordering::Relaxed);
                self.engine.connect().await?
            }
        };
        self.in_use.fetch_add(1, Ordering::Relaxed);
        Ok(Guard {
            conn: Some(conn),
            pool: self.clone(),
            _permit: permit,
            broken: false,
        })
    }
}
impl Guard {
    pub fn conn(&mut self) -> &mut dyn Conn {
        self.conn.as_deref_mut().unwrap()
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.pool.in_use.fetch_sub(1, Ordering::Relaxed);
        if !self.broken {
            if let Some(c) = self.conn.take() {
                self.pool.idle.lock().unwrap().push(c);
            }
        }
    }
}

// ---------------- メトリクス ----------------
#[derive(Clone, Serialize, Default)]
pub struct Point {
    pub t: f64,
    pub v: BTreeMap<String, f64>,
}

#[derive(Default)]
struct ActAcc {
    loops: u64,
    errors: u64,
    dropped: u64,
    lat: Option<Histogram<u64>>,
}
#[derive(Default)]
struct QAcc {
    count: u64,
    errors: u64,
    rows: u64,
    /// DB の実行時間（接続プールの待ちを除く。ネットワーク往復を含む）
    exec: Option<Histogram<u64>>,
    /// 接続プールで待った時間
    wait: Option<Histogram<u64>>,
}
#[derive(Default)]
struct Acc {
    actors: HashMap<String, ActAcc>,
    queries: HashMap<String, QAcc>,
    late: Option<Histogram<u64>>,
    cache_skips: u64,
}
/// 計測期間（ウォームアップ後）の通算
#[derive(Default)]
pub struct Totals {
    actors: HashMap<String, ActAcc>,
    queries: HashMap<String, QAcc>,
    late: Option<Histogram<u64>>,
}

pub struct Metrics {
    acc: Mutex<Acc>,
    tot: Mutex<Totals>,
    measuring: AtomicBool,
    pub series: Mutex<Vec<Point>>,
    pub tx: broadcast::Sender<Point>,
}
impl Metrics {
    pub fn new() -> Arc<Metrics> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Metrics {
            acc: Mutex::new(Acc::default()),
            tot: Mutex::new(Totals::default()),
            measuring: false.into(),
            series: Mutex::new(vec![]),
            tx,
        })
    }
    fn actor(&self, a: &str, f: impl Fn(&mut ActAcc)) {
        f(self.acc.lock().unwrap().actors.entry(a.into()).or_default());
        if self.measuring.load(Ordering::Relaxed) {
            f(self.tot.lock().unwrap().actors.entry(a.into()).or_default());
        }
    }
    fn query(&self, q: &str, f: impl Fn(&mut QAcc)) {
        f(self
            .acc
            .lock()
            .unwrap()
            .queries
            .entry(q.into())
            .or_default());
        if self.measuring.load(Ordering::Relaxed) {
            f(self
                .tot
                .lock()
                .unwrap()
                .queries
                .entry(q.into())
                .or_default());
        }
    }
    fn lateness(&self, d: Duration) {
        let f = |a: &mut Option<Histogram<u64>>| rec(a.get_or_insert_with(hist), d);
        f(&mut self.acc.lock().unwrap().late);
        if self.measuring.load(Ordering::Relaxed) {
            f(&mut self.tot.lock().unwrap().late);
        }
    }
    pub fn start_measuring(&self) {
        self.measuring.store(true, Ordering::Relaxed);
    }
}

// ---------------- 実行 ----------------
pub struct Ctx {
    pub project: Project,
    pub engine: Engine,
    pub reader: Option<Engine>,
    pub metrics: Arc<Metrics>,
    pub stop: Arc<Stop>,
    pub rows: BTreeMap<String, u64>,
    pub pools: HashMap<String, Arc<Pool>>,
    pub started: Instant,
    pub warmup: Duration,
    pub total_secs: f64,
    /// 直近の負荷倍率（f64 のビット）
    pub load_mult: AtomicU64,
}

/// プレースホルダの値を決める。`bound` が Some のときは、プリペア用に `$n` / `?` へ置き換えて値の列を返す。
pub fn render(sql: &str, tenant: u32, tenants: u32, rows: &BTreeMap<String, u64>, bind: &HashMap<String, i64>, bound: Option<EngineKind>) -> (String, Vec<i32>) {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut params: Vec<i32> = vec![];
    let mut rng = rand::thread_rng();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        let prev_colon = i > 0 && b[i - 1] == b':';
        let next_ident = i + 1 < b.len() && ((b[i + 1] as char).is_ascii_lowercase() || b[i + 1] == b'_');
        if c == ':' && !prev_colon && next_ident {
            let mut j = i + 1;
            while j < b.len() && ((b[j] as char).is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let name = &sql[i + 1..j];
            let v: i64 = if name == "tenant" {
                tenant as i64
            } else if name == "rand_any" {
                rng.gen_range(1..=1000) as i64 // 取り込んだクエリの値（本来の値は不明なので 1〜1000 の乱数）
            } else if name == "tenant_hi" {
                (tenant + 49).min(tenants) as i64
            } else if let Some(t) = name.strip_prefix("rand_") {
                let mut upper = rows.get(t).copied().unwrap_or(1).max(1);
                if let Some(h) = bind.get("__hot") {
                    upper = upper.min((*h).max(1) as u64); // ホットロウ: 先頭の K 件に集中
                }
                rng.gen_range(1..=upper) as i64
            } else {
                bind.get(name).copied().unwrap_or(1)
            };
            match bound {
                None => out.push_str(&v.to_string()),
                Some(k) => {
                    params.push(v as i32);
                    match k {
                        EngineKind::Postgres => out.push_str(&format!("${}::integer", params.len())),
                        EngineKind::Mysql => out.push('?'),
                    }
                }
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    (out, params)
}

/// プロジェクトの protocol に従って 1 クエリを実行する
pub async fn run_sql(conn: &mut dyn Conn, p: &Project, sql: &str, tenant: u32, rows: &BTreeMap<String, u64>, bind: &HashMap<String, i64>) -> anyhow::Result<crate::engine::RunRes> {
    match p.protocol {
        Protocol::Text => {
            let (s, _) = render(sql, tenant, p.tenants, rows, bind, None);
            conn.exec(&s).await
        }
        Protocol::Prepared | Protocol::PreparedEach => {
            let (s, params) = render(sql, tenant, p.tenants, rows, bind, Some(p.engine));
            conn.exec_params(&s, &params, p.protocol == Protocol::Prepared).await
        }
    }
}

async fn run_loop(ctx: Arc<Ctx>, actor: Arc<Actor>, tenant: u32, scheduled: Instant) {
    let pool = ctx.pools[&actor.name].clone();
    let rtt = if ctx.project.profile.net_mode == NetMode::Sleep { ctx.project.profile.rtt_ms } else { 0.0 };
    let mut results: HashMap<String, Vec<i64>> = HashMap::new();
    let mut bind: HashMap<String, i64> = HashMap::new();
    if let Some(h) = actor.hot_rows {
        bind.insert("__hot".into(), h as i64);
    }
    let mut failed = false;
    let mut held: Option<Guard> = None;
    if actor.tx {
        match pool.get().await {
            Ok(mut g) => {
                if g.conn().exec("BEGIN").await.is_err() {
                    g.broken = true;
                    failed = true;
                }
                held = Some(g);
            }
            Err(_) => failed = true,
        }
    }
    'steps: for step in &actor.steps {
        if failed {
            break;
        }
        let Some(q) = ctx.project.queries.get(&step.query) else {
            failed = true;
            break;
        };
        if q.write && !ctx.project.allow_writes {
            failed = true;
            break;
        }
        // 実行する回数と、それぞれの束縛
        let mut runs: Vec<Option<i64>> = vec![];
        match &step.foreach {
            Some(f) => {
                let ids = results.get(f).cloned().unwrap_or_default();
                for id in ids
                    .into_iter()
                    .take(step.limit.unwrap_or(u32::MAX) as usize)
                {
                    runs.push(Some(id));
                }
            }
            None => {
                for _ in 0..step.repeat.unwrap_or(1) {
                    runs.push(None);
                }
            }
        }
        for r in runs {
            if let Some(p) = step.cache_hit {
                if rand::thread_rng().gen::<f64>() < p {
                    ctx.metrics.acc.lock().unwrap().cache_skips += 1;
                    continue;
                }
            }
            if let (Some(f), Some(id)) = (&step.foreach, r) {
                bind.insert(f.clone(), id);
            }
            let mut wait_t = Duration::ZERO;
            let mut exec_t = Duration::ZERO;
            let res = if let Some(g) = held.as_mut() {
                let t = Instant::now();
                let r = run_sql(g.conn(), &ctx.project, &q.sql, tenant, &ctx.rows, &bind).await;
                exec_t = t.elapsed();
                r
            } else {
                let mut attempt = 0;
                loop {
                    let tw = Instant::now();
                    let got = pool.get().await;
                    wait_t += tw.elapsed();
                    let r = match got {
                        Ok(mut g) => {
                            let te = Instant::now();
                            let r = run_sql(g.conn(), &ctx.project, &q.sql, tenant, &ctx.rows, &bind).await;
                            exec_t += te.elapsed();
                            if r.is_err() {
                                g.broken = true; // 壊れた接続はプールに戻さない
                            }
                            if rtt > 0.0 {
                                tokio::time::sleep(Duration::from_secs_f64(rtt / 1000.0)).await;
                            }
                            r
                        }
                        Err(e) => Err(e),
                    };
                    if r.is_err() && ctx.project.retry_conn && attempt < 3 {
                        attempt += 1;
                        ctx.metrics.query("(retry)", |a| a.count += 1);
                        continue; // 新しい接続で 1 回だけやり直す
                    }
                    break r;
                }
            };
            if held.is_some() && rtt > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(rtt / 1000.0)).await;
            }
            let el = exec_t;
            match res {
                Ok(rr) => {
                    ctx.metrics.query(&step.query, |a| {
                        a.count += 1;
                        a.rows += rr.rows;
                        rec(a.exec.get_or_insert_with(hist), el);
                        rec(a.wait.get_or_insert_with(hist), wait_t);
                    });
                    if let Some(id) = &step.id {
                        results.insert(id.clone(), rr.ids.clone());
                        if let Some(first) = rr.ids.first() {
                            bind.insert(id.clone(), *first);
                        }
                    }
                }
                Err(_) => {
                    ctx.metrics.query(&step.query, |a| a.errors += 1);
                    failed = true;
                    break 'steps;
                }
            }
        }
    }
    if let Some(mut g) = held {
        let _ = g
            .conn()
            .exec(if failed { "ROLLBACK" } else { "COMMIT" })
            .await;
    }
    let lat = scheduled.elapsed();
    ctx.metrics.actor(&actor.name, |a| {
        if failed {
            a.errors += 1;
        } else {
            a.loops += 1;
            rec(a.lat.get_or_insert_with(hist), lat);
        }
    });
}

fn spawn_loop(
    ctx: &Arc<Ctx>,
    actor: &Arc<Actor>,
    inflight: &Arc<Semaphore>,
    tenant: u32,
    scheduled: Instant,
) {
    let Ok(permit) = inflight.clone().try_acquire_owned() else {
        ctx.metrics.actor(&actor.name, |a| a.dropped += 1); // 捌ききれず見送った（負荷側 / DB が詰まっている）
        return;
    };
    let (c, a) = (ctx.clone(), actor.clone());
    tokio::spawn(async move {
        run_loop(c, a, tenant, scheduled).await;
        drop(permit);
    });
}

async fn drive_actor(ctx: Arc<Ctx>, actor: Arc<Actor>) {
    let inflight = Arc::new(Semaphore::new(5000));
    let tenants = actor
        .tenant_limit
        .unwrap_or(ctx.project.tenants)
        .min(ctx.project.tenants)
        .max(1);
    match actor.kind {
        Kind::Worker => {
            let period = Duration::from_millis(actor.every_ms.unwrap_or(1000).max(1));
            let mut hs = vec![];
            for _inst in 0..actor.instances.max(1) {
                for tenant in 1..=tenants {
                    let (ctx, actor, inflight) = (ctx.clone(), actor.clone(), inflight.clone());
                    hs.push(tokio::spawn(async move {
                        let off = if actor.stagger {
                            Duration::from_secs_f64(
                                rand::thread_rng().gen::<f64>() * period.as_secs_f64(),
                            )
                        } else {
                            Duration::ZERO
                        };
                        let t0 = ctx.started + off + Duration::from_millis(200);
                        let mut k = 0u32;
                        while !ctx.stop.is_set() {
                            let at = t0 + period * k;
                            nap_until(&ctx.stop, at).await;
                            if ctx.stop.is_set() {
                                break;
                            }
                            ctx.metrics
                                .lateness(Instant::now().saturating_duration_since(at));
                            spawn_loop(&ctx, &actor, &inflight, tenant, at);
                            k += 1;
                        }
                    }));
                }
            }
            for h in hs {
                let _ = h.await;
            }
        }
        Kind::Web => {
            let base = tenants as f64 * actor.qps_per_tenant.unwrap_or(1.0);
            let zipf = actor.tenant_skew.filter(|s| *s > 0.0).map(|s| Zipf::new(tenants, s));
            let mut next = Instant::now();
            while !ctx.stop.is_set() {
                nap_until(&ctx.stop, next).await;
                let now = Instant::now();
                let mut guard = 0;
                while next <= now && guard < 5000 {
                    let tenant = pick_tenant(&zipf, tenants);
                    ctx.metrics.lateness(now.saturating_duration_since(next));
                    spawn_loop(&ctx, &actor, &inflight, tenant, next);
                    // 負荷倍率（計測開始からの秒。ウォームアップ中は 0 秒として扱う）
                    let t = (now.saturating_duration_since(ctx.started).as_secs_f64() - ctx.warmup.as_secs_f64()).max(0.0);
                    let mult = actor.shape.as_ref().map(|s| s.mult(t, ctx.total_secs)).unwrap_or(1.0).max(0.01);
                    ctx.load_mult.store(mult.to_bits(), Ordering::Relaxed);
                    let u: f64 = rand::thread_rng().gen::<f64>().max(1e-12);
                    next += Duration::from_secs_f64(-u.ln() / (base * mult).max(1e-6));
                    guard += 1;
                }
            }
        }
        Kind::Sse => {
            // 一斉接続: 初期スナップショットを全接続が取りに来る。every_ms があれば再接続の嵐を繰り返す
            let conns = actor.connections_per_tenant.unwrap_or(1).max(1);
            let period = actor.every_ms.map(|e| Duration::from_millis(e.max(1)));
            let big = Arc::new(Semaphore::new(100_000));
            let mut k = 0u32;
            loop {
                let at = ctx.started + ctx.warmup + Duration::from_millis(1000) + period.unwrap_or(Duration::ZERO) * k;
                nap_until(&ctx.stop, at).await;
                if ctx.stop.is_set() {
                    break;
                }
                for tenant in 1..=tenants {
                    // singleflight: テナントごとに 1 回だけ DB を引き、残りの接続は同じ結果を使う
                    let n = if actor.singleflight { 1 } else { conns };
                    for _ in 0..n {
                        ctx.metrics.lateness(Instant::now().saturating_duration_since(at));
                        spawn_loop(&ctx, &actor, &big, tenant, at);
                    }
                    if actor.singleflight && conns > 1 {
                        ctx.metrics.acc.lock().unwrap().cache_skips += (conns - 1) as u64;
                    }
                }
                k += 1;
                if period.is_none() {
                    break;
                }
            }
        }
        Kind::Batch => {
            let conc = actor.concurrency.unwrap_or(1).max(1);
            match actor.every_ms {
                Some(every) => {
                    let period = Duration::from_millis(every.max(1));
                    let zipf = actor.tenant_skew.filter(|s| *s > 0.0).map(|s| Zipf::new(tenants, s));
                    let mut k = 0u32;
                    while !ctx.stop.is_set() {
                        let at =
                            ctx.started + ctx.warmup + Duration::from_millis(1000) + period * k; // 計測期間に入るよう、ウォームアップ後に初回を打つ
                        nap_until(&ctx.stop, at).await;
                        if ctx.stop.is_set() {
                            break;
                        }
                        for _ in 0..conc {
                            let tenant = pick_tenant(&zipf, tenants);
                            spawn_loop(&ctx, &actor, &inflight, tenant, at);
                        }
                        k += 1;
                    }
                }
                None => {
                    let mut hs = vec![];
                    for _ in 0..conc {
                        let (ctx, actor) = (ctx.clone(), actor.clone());
                        hs.push(tokio::spawn(async move {
                            while !ctx.stop.is_set() {
                                let tenant = rand::thread_rng().gen_range(1..=tenants);
                                run_loop(ctx.clone(), actor.clone(), tenant, Instant::now()).await;
                            }
                        }));
                    }
                    for h in hs {
                        let _ = h.await;
                    }
                }
            }
        }
    }
}

// ---------------- テナントの選び方（Zipf） ----------------
struct Zipf {
    cdf: Vec<f64>,
}
impl Zipf {
    fn new(n: u32, s: f64) -> Zipf {
        let mut acc = 0.0;
        let mut cdf = Vec::with_capacity(n as usize);
        for k in 1..=n.max(1) {
            acc += 1.0 / (k as f64).powf(s);
            cdf.push(acc);
        }
        let t = acc;
        cdf.iter_mut().for_each(|x| *x /= t);
        Zipf { cdf }
    }
    fn sample(&self) -> u32 {
        let u: f64 = rand::thread_rng().gen();
        (self.cdf.partition_point(|&c| c < u) as u32 + 1).min(self.cdf.len() as u32)
    }
}
fn pick_tenant(z: &Option<Zipf>, n: u32) -> u32 {
    match z {
        Some(z) => z.sample(),
        None => rand::thread_rng().gen_range(1..=n),
    }
}

// ---------------- 結果 ----------------
#[derive(Serialize, Clone, Default)]
pub struct ActorSummary {
    pub name: String,
    pub loops_per_s: f64,
    pub errors: u64,
    pub dropped: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}
#[derive(Serialize, Clone, Default)]
pub struct Conclusion {
    pub verdict: String,
    pub slo_ok: bool,
    pub bottleneck: String,
    pub messages: Vec<String>,
    pub cpu_cores_avg: Option<f64>,
    pub cpu_util: Option<f64>,
    pub safe_tenants_extrapolated: Option<f64>,
    pub pool_wait_max: f64,
    pub lateness_p99_ms: f64,
    pub gen_cpu_pct: f64,
}
#[derive(Serialize, Clone, Default)]
pub struct Summary {
    pub actors: Vec<ActorSummary>,
    pub queries: BTreeMap<String, serde_json::Value>,
    pub conclusion: Conclusion,
    pub measured_secs: f64,
}

#[derive(Clone)]
pub struct RunOpts {
    pub secs: u64,
    pub warmup_secs: u64,
}

pub async fn execute(
    project: Project,
    engine: Engine,
    reader: Option<Engine>,
    cpu_src: CpuSrc,
    opts: RunOpts,
    metrics: Arc<Metrics>,
    stop: Arc<Stop>,
    events: Arc<Mutex<Vec<(f64, String)>>>,
) -> Summary {
    let rows = schema::rows_per_tenant(&project);
    let mut pools = HashMap::new();
    for a in &project.actors {
        let eng = if a.target == Target::Reader { reader.clone().unwrap_or_else(|| engine.clone()) } else { engine.clone() };
        pools.insert(a.name.clone(), Pool::new(eng, a.pool_max_open));
    }
    let started = Instant::now();
    let ctx = Arc::new(Ctx {
        project: project.clone(),
        engine: engine.clone(),
        reader: reader.clone(),
        metrics: metrics.clone(),
        stop: stop.clone(),
        rows,
        pools,
        started,
        warmup: Duration::from_secs(opts.warmup_secs),
        total_secs: opts.secs as f64,
        load_mult: AtomicU64::new(1.0f64.to_bits()),
    });
    let ev = |s: &str| {
        events
            .lock()
            .unwrap()
            .push((started.elapsed().as_secs_f64(), s.to_string()))
    };
    ev("開始");

    // 採取（1 秒ごと）
    let sampler = {
        let ctx = ctx.clone();
        let cpu_src = cpu_src.clone();
        tokio::spawn(async move {
            let mut sc = ctx.engine.connect_direct().await.ok();
            let mut last_cpu = cpu_src.read();
            let mut last_self = cpu::self_cpu();
            let mut last_snap: BTreeMap<String, f64> = BTreeMap::new();
            let mut last_opened = 0u64;
            let mut pobs = ctx.project.proxy.clone().map(crate::proxy::ProxyObserver::new);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            while !ctx.stop.is_set() {
                tick.tick().await;
                let mut v = BTreeMap::new();
                {
                    let mut acc = ctx.metrics.acc.lock().unwrap();
                    v.insert(
                        "errors".into(),
                        acc.actors.values().map(|a| a.errors).sum::<u64>() as f64,
                    );
                    v.insert(
                        "dropped".into(),
                        acc.actors.values().map(|a| a.dropped).sum::<u64>() as f64,
                    );
                    for (name, a) in acc.actors.iter_mut() {
                        v.insert(format!("loops.{name}"), a.loops as f64);
                        v.insert(format!("err.{name}"), a.errors as f64);
                        v.insert(format!("dropped.{name}"), a.dropped as f64);
                        if let Some(h) = &a.lat {
                            v.insert(format!("p50.{name}"), ms(h, 0.5));
                            v.insert(format!("p95.{name}"), ms(h, 0.95));
                            v.insert(format!("p99.{name}"), ms(h, 0.99));
                        }
                        *a = ActAcc::default();
                    }
                    for (name, q) in acc.queries.iter_mut() {
                        v.insert(format!("q.{name}.qps"), q.count as f64);
                        if let Some(h) = &q.exec {
                            v.insert(format!("q.{name}.p95"), ms(h, 0.95));
                        }
                        if let Some(h) = &q.wait {
                            v.insert(format!("q.{name}.wait_p95"), ms(h, 0.95));
                        }
                        *q = QAcc::default();
                    }
                    let (te, td) = (
                        acc.actors.values().map(|a| a.errors).sum::<u64>(),
                        acc.actors.values().map(|a| a.dropped).sum::<u64>(),
                    );
                    let _ = (te, td);
                    if let Some(h) = &acc.late {
                        v.insert("lateness.p99".into(), ms(h, 0.99));
                    }
                    acc.late = None;
                    v.insert("cache_skips".into(), acc.cache_skips as f64);
                    acc.cache_skips = 0;
                }
                let mut pool_wait = 0.0;
                for (name, p) in &ctx.pools {
                    v.insert(
                        format!("pool.{name}.in_use"),
                        p.in_use.load(Ordering::Relaxed) as f64,
                    );
                    let w = p.waiting.load(Ordering::Relaxed) as f64;
                    v.insert(format!("pool.{name}.waiting"), w);
                    pool_wait += w;
                }
                v.insert("pool.waiting".into(), pool_wait);
                v.insert("load.mult".into(), f64::from_bits(ctx.load_mult.load(Ordering::Relaxed)));
                if sc.is_none() {
                    sc = ctx.engine.connect_direct().await.ok();
                }
                let mut snap_failed = false;
                if let Some(c) = sc.as_mut() {
                    let snap_r = ctx.engine.snapshot(c.as_mut()).await;
                    if snap_r.is_err() {
                        snap_failed = true;
                    }
                    if let Ok(snap) = snap_r {
                        for k in ["db.conns", "db.active"] {
                            if let Some(x) = snap.get(k) {
                                v.insert(k.into(), *x);
                            }
                        }
                        let d = |k: &str| {
                            snap.get(k).copied().unwrap_or(0.0)
                                - last_snap
                                    .get(k)
                                    .copied()
                                    .unwrap_or(snap.get(k).copied().unwrap_or(0.0))
                        };
                        let (h, r) = (d("db.blks_hit"), d("db.blks_read"));
                        if h + r > 0.0 {
                            v.insert("db.cache_hit".into(), h / (h + r));
                        }
                        v.insert("db.disk_reads".into(), r.max(0.0));
                        v.insert("db.rows_read".into(), d("db.rows_read"));
                        v.insert("db.lock_waits".into(), d("db.lock_waits"));
                        last_snap = snap;
                    }
                }
                if snap_failed {
                    sc = None; // 切断された。次の周期で張り直す
                }
                let opened: u64 = ctx.pools.values().map(|p| p.opened.load(Ordering::Relaxed)).sum();
                v.insert("pool.reconnects".into(), opened.saturating_sub(last_opened) as f64);
                last_opened = opened;
                if let Some(po) = pobs.as_mut() {
                    if let Ok(pm) = po.sample().await {
                        v.extend(pm);
                    }
                }
                let now_cpu = cpu_src.read();
                if let (Some(a), Some(b)) = (now_cpu, last_cpu) {
                    let cores = (a - b).max(0.0);
                    v.insert("db.cpu_cores".into(), cores);
                    v.insert("db.cpu_util".into(), cores / ctx.project.profile.vcpu);
                }
                last_cpu = now_cpu;
                let s = cpu::self_cpu();
                v.insert("gen.rss_mb".into(), cpu::self_rss_mb());
                v.insert("gen.cpu_cores".into(), (s - last_self).max(0.0));
                last_self = s;
                let pt = Point {
                    t: ctx.started.elapsed().as_secs_f64(),
                    v,
                };
                ctx.metrics.series.lock().unwrap().push(pt.clone());
                let _ = ctx.metrics.tx.send(pt);
            }
        })
    };

    // 障害注入
    let mut chaos_hs = vec![];
    for ch in project.chaos.clone() {
        let (ctx, events) = (ctx.clone(), events.clone());
        chaos_hs.push(tokio::spawn(async move {
            let at = ctx.started + Duration::from_secs(ctx.warmup.as_secs()) + Duration::from_secs_f64(ch.at_s.max(0.0));
            nap_until(&ctx.stop, at).await;
            if ctx.stop.is_set() {
                return;
            }
            let ChaosAction::KillConnections = ch.action;
            let msg = match ctx.engine.kill_connections().await {
                Ok(n) => format!("切断 {n} 本"),
                Err(e) => format!("切断に失敗: {e}"),
            };
            events.lock().unwrap().push((ctx.started.elapsed().as_secs_f64(), msg));
        }));
    }
    let mut hs = vec![];
    for a in &project.actors {
        hs.push(tokio::spawn(drive_actor(ctx.clone(), Arc::new(a.clone()))));
    }

    // ウォームアップ → 計測 → 終了
    let deadline = started + Duration::from_secs(opts.warmup_secs + opts.secs);
    let mut measuring = false;
    while Instant::now() < deadline && !stop.is_set() {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !measuring && started.elapsed() >= Duration::from_secs(opts.warmup_secs) {
            measuring = true;
            metrics.start_measuring();
            ev("計測");
        }
    }
    stop.set();
    ev("終了");
    for h in hs {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
    for h in chaos_hs {
        let _ = h.await;
    }
    let _ = sampler.await;
    let evs = events.lock().unwrap().clone();
    summarize(&ctx, &opts, &evs)
}

fn summarize(ctx: &Ctx, opts: &RunOpts, evs: &[(f64, String)]) -> Summary {
    let p = &ctx.project;
    let tot = ctx.metrics.tot.lock().unwrap();
    let secs = opts.secs as f64;
    let mut s = Summary {
        measured_secs: secs,
        ..Default::default()
    };
    let mut worst_p95 = 0.0f64;
    let (mut loops, mut errs, mut drops) = (0u64, 0u64, 0u64);
    for a in &p.actors {
        let t = tot.actors.get(&a.name);
        let h = t.and_then(|x| x.lat.as_ref());
        let as_ = ActorSummary {
            name: a.name.clone(),
            loops_per_s: t.map(|x| x.loops as f64).unwrap_or(0.0) / secs,
            errors: t.map(|x| x.errors).unwrap_or(0),
            dropped: t.map(|x| x.dropped).unwrap_or(0),
            p50_ms: h.map(|h| ms(h, 0.5)).unwrap_or(0.0),
            p95_ms: h.map(|h| ms(h, 0.95)).unwrap_or(0.0),
            p99_ms: h.map(|h| ms(h, 0.99)).unwrap_or(0.0),
        };
        if a.kind != Kind::Batch {
            worst_p95 = worst_p95.max(as_.p95_ms);
        }
        loops += t.map(|x| x.loops).unwrap_or(0);
        errs += as_.errors;
        drops += as_.dropped;
        s.actors.push(as_);
    }
    for (q, a) in &tot.queries {
        let h = a.exec.as_ref();
        s.queries.insert(q.clone(), serde_json::json!({
            "qps": a.count as f64 / secs, "errors": a.errors,
            "avg_rows": if a.count > 0 { a.rows as f64 / a.count as f64 } else { 0.0 },
            "p50_ms": h.map(|h| ms(h, 0.5)).unwrap_or(0.0), "p95_ms": h.map(|h| ms(h, 0.95)).unwrap_or(0.0),
                "wait_p50_ms": a.wait.as_ref().map(|h| ms(h, 0.5)).unwrap_or(0.0), "wait_p95_ms": a.wait.as_ref().map(|h| ms(h, 0.95)).unwrap_or(0.0),
        }));
    }
    // 計測期間の時系列から平均を出す
    let series = ctx.metrics.series.lock().unwrap();
    let warm = opts.warmup_secs as f64;
    let pts: Vec<&Point> = series.iter().filter(|x| x.t >= warm).collect();
    let avg = |k: &str| -> Option<f64> {
        let v: Vec<f64> = pts.iter().filter_map(|x| x.v.get(k).copied()).collect();
        if v.is_empty() {
            None
        } else {
            Some(v.iter().sum::<f64>() / v.len() as f64)
        }
    };
    let maxv = |k: &str| {
        pts.iter()
            .filter_map(|x| x.v.get(k).copied())
            .fold(0.0f64, f64::max)
    };
    let cpu_cores = avg("db.cpu_cores");
    let util = cpu_cores.map(|c| c / p.profile.vcpu);
    let late = tot.late.as_ref().map(|h| ms(h, 0.99)).unwrap_or(0.0);
    let gen_pct = avg("gen.cpu_cores").unwrap_or(0.0) * 100.0;
    let pool_wait_max = maxv("pool.waiting");

    let mut c = Conclusion {
        cpu_cores_avg: cpu_cores,
        cpu_util: util,
        pool_wait_max,
        lateness_p99_ms: late,
        gen_cpu_pct: gen_pct,
        ..Default::default()
    };
    let total_q: f64 = s
        .queries
        .values()
        .map(|v| v["qps"].as_f64().unwrap_or(0.0))
        .sum();
    let err_rate = if loops + errs > 0 {
        errs as f64 / (loops + errs) as f64
    } else {
        0.0
    };
    c.slo_ok = worst_p95 <= p.slo.p95_ms && err_rate <= p.slo.error_rate && drops == 0;
    if err_rate > p.slo.error_rate {
        c.bottleneck = "エラー".into();
        c.messages.push(format!("エラー率 {:.2}% が基準 {:.2}% を超えています（接続数の上限・タイムアウト・権限などを確認）", err_rate * 100.0, p.slo.error_rate * 100.0));
    }
    if gen_pct > 80.0 * 1.0 {
        c.bottleneck = "負荷側が詰まっている".into();
        c.messages.push(format!("負荷生成側の CPU が {gen_pct:.0}% です。測定が信用できません（DB と別のコア / ホストに分けてください）"));
    } else if drops > 0 || late > 1000.0 {
        c.bottleneck = "負荷を捌ききれていない".into();
        c.messages.push(format!("見送ったループ {drops} 件、スケジュール遅延 p99 {late:.0}ms。計画した負荷が出ていません"));
    } else if pool_wait_max > 0.0 && util.map(|u| u < p.profile.safe_util).unwrap_or(true) {
        c.bottleneck = "接続プール（CPU には余裕あり）".into();
        c.messages.push(format!("プールで最大 {pool_wait_max:.0} 件が待たされています。CPU 使用率は低いのに遅いなら、プール上限・トランザクションの保持時間（外部待ち）を疑ってください"));
    } else if let Some(u) = util {
        if u > 0.88 {
            c.bottleneck = "CPU（崩壊域）".into();
            c.messages.push(format!(
                "CPU 使用率 {:.0}%。待ち行列が急に伸びる領域です",
                u * 100.0
            ));
        } else if u > p.profile.safe_util {
            c.bottleneck = "CPU（安全枠超過）".into();
            c.messages.push(format!(
                "CPU 使用率 {:.0}% が安全枠 {:.0}% を超えています",
                u * 100.0,
                p.profile.safe_util * 100.0
            ));
        }
    }
    if c.bottleneck.is_empty() {
        c.bottleneck = if util.is_some() {
            "余裕あり".into()
        } else {
            "不明（CPU を採取できていない）".into()
        };
    }
    let other_limit = c.bottleneck.starts_with("接続プール")
        || c.bottleneck.starts_with("負荷")
        || c.bottleneck == "エラー";
    if other_limit {
        c.messages.push("CPU 以外の限界が先に来ているため、CPU からの安全上限の外挿はしません（上の原因を解消してから測り直してください）".into());
    } else if let Some(cores) = cpu_cores {
        let batch_fixed = 0.0;
        let per_tenant = (cores - batch_fixed) / p.tenants.max(1) as f64;
        if per_tenant > 0.0 {
            let safe = (p.profile.vcpu * p.profile.safe_util - p.profile.reserve_vcpu) / per_tenant;
            c.safe_tenants_extrapolated = Some(safe.max(0.0));
            c.messages.push(format!("実測 CPU {cores:.2} vCPU / {} テナント → 安全上限は約 {:.0} テナント（線形に外挿。{:.0}% で曲がるため目安）", p.tenants, safe.max(0.0), 88.0));
        }
    } else {
        c.messages.push("CPU を採取できていないため、安全上限の外挿はできません（CAPLAB_CPU_CGROUP を設定するか、DB と同じホストで実行してください）".into());
    }
    c.messages.push(format!(
        "1 秒あたり {total_q:.0} クエリ、{} ループ/秒",
        (loops as f64 / secs) as u64
    ));
    // ソーク: 計測期間の前半と後半で、遅延・接続・メモリが漂っていないか（30 秒以上の計測で評価）
    if opts.secs >= 30 && pts.len() >= 12 {
        let third = pts.len() / 3;
        let mean_of = |sl: &[&Point], f: &dyn Fn(&Point) -> Option<f64>| -> Option<f64> {
            let v: Vec<f64> = sl.iter().filter_map(|x| f(x)).collect();
            if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) }
        };
        let worst = |x: &Point| -> Option<f64> {
            let v: Vec<f64> = x.v.iter().filter(|(k, _)| k.starts_with("p95.") && !k.contains("batch")).map(|(_, v)| *v).collect();
            if v.is_empty() { None } else { Some(v.into_iter().fold(0.0, f64::max)) }
        };
        let (first, last) = (&pts[..third], &pts[pts.len() - third..]);
        let checks: [(&str, Box<dyn Fn(&Point) -> Option<f64>>, f64); 4] = [
            ("p95", Box::new(worst), 1.25),
            ("DB の接続数", Box::new(|x: &Point| x.v.get("db.conns").copied()), 1.2),
            ("負荷側のメモリ", Box::new(|x: &Point| x.v.get("gen.rss_mb").copied()), 1.2),
            ("DB の CPU", Box::new(|x: &Point| x.v.get("db.cpu_cores").copied()), 1.25),
        ];
        let mut any = false;
        for (name, f, thr) in checks.iter() {
            if let (Some(a), Some(b)) = (mean_of(first, f.as_ref()), mean_of(last, f.as_ref())) {
                if a > 0.0 && b / a > *thr {
                    any = true;
                    c.messages.push(format!("ドリフト: {name} が計測の前半 {a:.2} → 後半 {b:.2}（{:+.0}%）。長時間で悪化しています（リーク・肥大・キャッシュの冷えを疑う）", (b / a - 1.0) * 100.0));
                }
            }
        }
        if !any {
            c.messages.push(format!("ドリフトなし（{} 秒の計測の前半と後半で p95・接続・メモリ・CPU に目立った増加なし）", opts.secs));
        }
    }
    // 障害からの回復時間
    for (t_e, txt) in evs.iter().filter(|(_, t)| t.starts_with("切断")) {
        let worst = |x: &Point| x.v.iter().filter(|(k, _)| k.starts_with("p95.") && !k.contains("batch")).map(|(_, v)| *v).fold(0.0f64, f64::max);
        let errs = |x: &Point| x.v.get("errors").copied().unwrap_or(0.0);
        let mut base: Vec<f64> = series.iter().filter(|x| x.t >= warm && x.t < *t_e).map(|x| worst(x)).collect();
        base.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let b = if base.is_empty() { 0.0 } else { base[base.len() / 2] };
        let thr = (b * 1.5).max(b + 5.0);
        let after: Vec<&Point> = series.iter().filter(|x| x.t >= *t_e).collect();
        let mut rec_s = None;
        for i in 0..after.len() {
            let ok = after[i..].iter().take(3).all(|x| worst(x) <= thr && errs(x) == 0.0);
            if ok && after.len() - i >= 3 {
                rec_s = Some(after[i].t - t_e);
                break;
            }
        }
        let peak = after.iter().take(30).map(|x| worst(x)).fold(0.0f64, f64::max);
        let nerr: f64 = after.iter().take(30).map(|x| errs(x)).sum();
        c.messages.push(format!("障害注入（{txt}、{t_e:.0}秒）: 回復まで {}、p95 最大 {peak:.0}ms（平常 {b:.1}ms）、エラー {nerr:.0} 件{}", match rec_s { Some(s) => format!("{s:.0} 秒"), None => "（計測期間内に回復せず）".into() }, if ctx.project.retry_conn { "。接続エラーは 1 回やり直し済み" } else { "" }));
    }
    c.verdict = if c.slo_ok {
        format!(
            "合格（p95 {worst_p95:.1}ms ≤ {}ms、エラー率 {:.2}%）",
            p.slo.p95_ms,
            err_rate * 100.0
        )
    } else {
        format!(
            "不合格（p95 {worst_p95:.1}ms / 基準 {}ms、エラー率 {:.2}%、見送り {drops}）",
            p.slo.p95_ms,
            err_rate * 100.0
        )
    };
    s.conclusion = c;
    s
}

// ---------------- 校正 ----------------
pub async fn calibrate(
    project: &Project,
    engine: &Engine,
    cpu_src: &CpuSrc,
    conc: u32,
    secs_each: u64,
    stop: Arc<Stop>,
    progress: impl Fn(f64, &str),
) -> Calib {
    let rows = schema::rows_per_tenant(project);
    let names: Vec<String> = project
        .queries
        .iter()
        .filter(|(_, q)| !q.write || project.allow_writes)
        .map(|(n, _)| n.clone())
        .collect();
    let mut out = Calib {
        concurrency: conc,
        ..Default::default()
    };
    let pool = Pool::new(engine.clone(), conc);
    for (i, name) in names.iter().enumerate() {
        if stop.is_set() {
            break;
        }
        progress(i as f64 / names.len().max(1) as f64, name);
        let q = project.queries[name].clone();
        let count = Arc::new(AtomicU64::new(0));
        let rowsum = Arc::new(AtomicU64::new(0));
        let h = Arc::new(Mutex::new(hist()));
        let end = Instant::now() + Duration::from_secs(secs_each);
        let c0 = cpu_src.read();
        let mut hs = vec![];
        for _ in 0..conc {
            let (pool, q, rows, count, rowsum, h) = (
                pool.clone(),
                q.clone(),
                rows.clone(),
                count.clone(),
                rowsum.clone(),
                h.clone(),
            );
            let tenants = project.tenants;
            let proj = project.clone();
            hs.push(tokio::spawn(async move {
                while Instant::now() < end {
                    let tenant = rand::thread_rng().gen_range(1..=tenants.max(1));
                    let Ok(mut g) = pool.get().await else { break };
                    let t = Instant::now();
                    match run_sql(g.conn(), &proj, &q.sql, tenant, &rows, &HashMap::new()).await {
                        Ok(r) => {
                            rec(&mut h.lock().unwrap(), t.elapsed());
                            count.fetch_add(1, Ordering::Relaxed);
                            rowsum.fetch_add(r.rows, Ordering::Relaxed);
                        }
                        Err(_) => {
                            g.broken = true;
                            break;
                        }
                    }
                }
            }));
        }
        for x in hs {
            let _ = x.await;
        }
        let n = count.load(Ordering::Relaxed) as f64;
        if n > 0.0 {
            if let (Some(a), Some(b)) = (cpu_src.read(), c0) {
                out.cpu_ms.insert(name.clone(), (a - b) / n * 1000.0);
            }
            out.avg_rows
                .insert(name.clone(), rowsum.load(Ordering::Relaxed) as f64 / n);
            out.p50_ms.insert(name.clone(), ms(&h.lock().unwrap(), 0.5));
        }
    }
    out
}
