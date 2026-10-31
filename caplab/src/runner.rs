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
    exec: Option<Histogram<u64>>,
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
    pub metrics: Arc<Metrics>,
    pub stop: Arc<Stop>,
    pub rows: BTreeMap<String, u64>,
    pub pools: HashMap<String, Arc<Pool>>,
    pub started: Instant,
    pub warmup: Duration,
}

fn resolve(
    sql: &str,
    tenant: u32,
    tenants: u32,
    rows: &BTreeMap<String, u64>,
    bind: &HashMap<String, i64>,
) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut rng = rand::thread_rng();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        let prev_colon = i > 0 && b[i - 1] == b':';
        let next_ident =
            i + 1 < b.len() && ((b[i + 1] as char).is_ascii_lowercase() || b[i + 1] == b'_');
        if c == ':' && !prev_colon && next_ident && !(i + 1 < b.len() && b[i + 1] == b':') {
            let mut j = i + 1;
            while j < b.len() && ((b[j] as char).is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let name = &sql[i + 1..j];
            let v: i64 = if name == "tenant" {
                tenant as i64
            } else if name == "tenant_hi" {
                (tenant + 49).min(tenants) as i64
            } else if let Some(t) = name.strip_prefix("rand_") {
                rng.gen_range(1..=rows.get(t).copied().unwrap_or(1).max(1)) as i64
            } else {
                bind.get(name).copied().unwrap_or(1)
            };
            out.push_str(&v.to_string());
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

async fn run_loop(ctx: Arc<Ctx>, actor: Arc<Actor>, tenant: u32, scheduled: Instant) {
    let pool = ctx.pools[&actor.name].clone();
    let rtt = ctx.project.profile.rtt_ms;
    let mut results: HashMap<String, Vec<i64>> = HashMap::new();
    let mut bind: HashMap<String, i64> = HashMap::new();
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
            let sql = resolve(&q.sql, tenant, ctx.project.tenants, &ctx.rows, &bind);
            let t = Instant::now();
            let res = if let Some(g) = held.as_mut() {
                g.conn().exec(&sql).await
            } else {
                match pool.get().await {
                    Ok(mut g) => {
                        let r = g.conn().exec(&sql).await;
                        if r.is_err() {
                            g.broken = true;
                        }
                        if rtt > 0.0 {
                            tokio::time::sleep(Duration::from_secs_f64(rtt / 1000.0)).await;
                        }
                        r
                    }
                    Err(e) => Err(e),
                }
            };
            if held.is_some() && rtt > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(rtt / 1000.0)).await;
            }
            let el = t.elapsed();
            match res {
                Ok(rr) => {
                    ctx.metrics.query(&step.query, |a| {
                        a.count += 1;
                        a.rows += rr.rows;
                        rec(a.exec.get_or_insert_with(hist), el);
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
            let rate = tenants as f64 * actor.qps_per_tenant.unwrap_or(1.0);
            let mut next = Instant::now();
            while !ctx.stop.is_set() {
                nap_until(&ctx.stop, next).await;
                let now = Instant::now();
                let mut guard = 0;
                while next <= now && guard < 5000 {
                    let tenant = rand::thread_rng().gen_range(1..=tenants);
                    ctx.metrics.lateness(now.saturating_duration_since(next));
                    spawn_loop(&ctx, &actor, &inflight, tenant, next);
                    let u: f64 = rand::thread_rng().gen::<f64>().max(1e-12);
                    next += Duration::from_secs_f64(-u.ln() / rate);
                    guard += 1;
                }
            }
        }
        Kind::Batch => {
            let conc = actor.concurrency.unwrap_or(1).max(1);
            match actor.every_ms {
                Some(every) => {
                    let period = Duration::from_millis(every.max(1));
                    let mut k = 0u32;
                    while !ctx.stop.is_set() {
                        let at =
                            ctx.started + ctx.warmup + Duration::from_millis(1000) + period * k; // 計測期間に入るよう、ウォームアップ後に初回を打つ
                        nap_until(&ctx.stop, at).await;
                        if ctx.stop.is_set() {
                            break;
                        }
                        for _ in 0..conc {
                            let tenant = rand::thread_rng().gen_range(1..=tenants);
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
    cpu_src: CpuSrc,
    opts: RunOpts,
    metrics: Arc<Metrics>,
    stop: Arc<Stop>,
    events: Arc<Mutex<Vec<(f64, String)>>>,
) -> Summary {
    let rows = schema::rows_per_tenant(&project);
    let mut pools = HashMap::new();
    for a in &project.actors {
        pools.insert(a.name.clone(), Pool::new(engine.clone(), a.pool_max_open));
    }
    let started = Instant::now();
    let ctx = Arc::new(Ctx {
        project: project.clone(),
        engine: engine.clone(),
        metrics: metrics.clone(),
        stop: stop.clone(),
        rows,
        pools,
        started,
        warmup: Duration::from_secs(opts.warmup_secs),
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
            let mut sc = ctx.engine.connect().await.ok();
            let mut last_cpu = cpu_src.read();
            let mut last_self = cpu::self_cpu();
            let mut last_snap: BTreeMap<String, f64> = BTreeMap::new();
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
                if let Some(c) = sc.as_mut() {
                    if let Ok(snap) = ctx.engine.snapshot(c.as_mut()).await {
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
                        v.insert("db.rows_read".into(), d("db.rows_read"));
                        v.insert("db.lock_waits".into(), d("db.lock_waits"));
                        last_snap = snap;
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
    let _ = sampler.await;
    summarize(&ctx, &opts)
}

fn summarize(ctx: &Ctx, opts: &RunOpts) -> Summary {
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
            hs.push(tokio::spawn(async move {
                while Instant::now() < end {
                    let tenant = rand::thread_rng().gen_range(1..=tenants.max(1));
                    let sql = resolve(&q.sql, tenant, tenants, &rows, &HashMap::new());
                    let Ok(mut g) = pool.get().await else { break };
                    let t = Instant::now();
                    match g.conn().exec(&sql).await {
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
