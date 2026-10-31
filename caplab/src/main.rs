mod cpu;
mod engine;
mod estimate;
mod import;
mod model;
mod net;
mod proxy;
mod runner;
mod schema;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use engine::Engine;
use futures::StreamExt;
use model::*;
use runner::{Metrics, RunOpts, Stop, Summary};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio_stream::wrappers::BroadcastStream;

type Res<T> = Result<T, (StatusCode, String)>;
fn bad<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

#[derive(Serialize, Clone, Default)]
struct Job {
    name: String,
    step: String,
    progress: f64,
    running: bool,
    error: Option<String>,
    run_id: Option<String>,
}

struct Run {
    id: String,
    project: Project,
    metrics: Arc<Metrics>,
    stop: Arc<Stop>,
    events: Arc<Mutex<Vec<(f64, String)>>>,
    summary: Mutex<Option<Summary>>,
    state: Mutex<String>,
    settings: Mutex<BTreeMap<String, String>>,
    notes: Mutex<Vec<String>>,
    cpu_src: String,
    opts: (u64, u64),
    estimate: serde_json::Value,
    started_unix: f64,
}

struct App {
    dir: PathBuf,
    project: RwLock<Project>,
    calib: RwLock<Calib>,
    runs: Mutex<BTreeMap<String, Arc<Run>>>,
    job: Mutex<Job>,
    /// 掃引・繰り返しの結果
    results: Mutex<BTreeMap<String, serde_json::Value>>,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

// ---------- 接続先のガード ----------
fn dsn_of(p: &Project) -> String {
    if !p.dsn.is_empty() {
        p.dsn.clone()
    } else {
        std::env::var("CAPLAB_DSN").unwrap_or_default()
    }
}
fn dsn_host(dsn: &str) -> String {
    if let Some(rest) = dsn.split("://").nth(1) {
        let hostport = rest
            .rsplit('@')
            .next()
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("");
        return hostport.split(':').next().unwrap_or("").to_string();
    }
    dsn.split_whitespace()
        .find_map(|t| t.strip_prefix("host="))
        .unwrap_or("localhost")
        .to_string()
}
fn is_local(dsn: &str) -> bool {
    matches!(
        dsn_host(dsn).as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}
fn engine_for(p: &Project) -> Result<Engine, String> {
    let dsn = dsn_of(p);
    if dsn.is_empty() {
        return Err("接続先が未設定です（プロジェクトの dsn か環境変数 CAPLAB_DSN）".into());
    }
    if !is_local(&dsn) && std::env::var("CAPLAB_ALLOW_REMOTE").ok().as_deref() != Some("1") {
        return Err(format!("接続先 '{}' はローカルではありません。実験用 DB であることを確認して CAPLAB_ALLOW_REMOTE=1 を設定してください（本番には向けないでください）", dsn_host(&dsn)));
    }
    if p.engine == EngineKind::Mysql {
        let db = dsn
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').nth(1))
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("");
        if !db.starts_with("caplab") {
            return Err(format!("MySQL のデータベース名は 'caplab' で始めてください（既存のデータを壊さないため。現在: '{db}'）"));
        }
    }
    Ok(Engine::new(p.engine, dsn).with_tls(p.tls))
}

fn save(app: &App) {
    let _ = std::fs::create_dir_all(&app.dir);
    let _ = std::fs::write(
        app.dir.join("project.json"),
        serde_json::to_vec_pretty(&*app.project.read().unwrap()).unwrap(),
    );
    let _ = std::fs::write(
        app.dir.join("calib.json"),
        serde_json::to_vec_pretty(&*app.calib.read().unwrap()).unwrap(),
    );
}

// ---------- ハンドラ ----------
async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}
async fn get_project(State(a): State<Arc<App>>) -> Json<Project> {
    Json(a.project.read().unwrap().clone())
}
async fn put_project(State(a): State<Arc<App>>, Json(p): Json<Project>) -> Res<Json<Project>> {
    schema::validate(&p).map_err(bad)?;
    *a.project.write().unwrap() = p.clone();
    save(&a);
    Ok(Json(p))
}
async fn post_simple(
    State(a): State<Arc<App>>,
    Json(s): Json<schema::Simple>,
) -> Res<Json<Project>> {
    let mut p = a.project.read().unwrap().clone();
    if p.schema.tables.is_empty() {
        return Err(bad("先にテーブルを作ってください（親・子・孫）"));
    }
    schema::validate(&p).map_err(bad)?;
    schema::apply_simple(&mut p, &s);
    *a.project.write().unwrap() = p.clone();
    save(&a);
    Ok(Json(p))
}
async fn get_candidates(State(a): State<Arc<App>>) -> Json<BTreeMap<String, QueryDef>> {
    Json(schema::candidates(&a.project.read().unwrap()))
}
async fn get_ddl(State(a): State<Arc<App>>) -> Json<Vec<String>> {
    Json(schema::ddl(&a.project.read().unwrap()))
}
#[derive(Deserialize)]
struct EstQ {
    tenants: Option<u32>,
    rtt: Option<f64>,
}
async fn get_estimate(
    State(a): State<Arc<App>>,
    Query(q): Query<EstQ>,
) -> Json<estimate::Estimate> {
    let mut p = a.project.read().unwrap().clone();
    if let Some(r) = q.rtt {
        p.profile.rtt_ms = r;
        p.profile.measured_rtt_ms = None;
    }
    let c = a.calib.read().unwrap().clone();
    Json(estimate::estimate(&p, &c, q.tenants.unwrap_or(p.tenants)))
}
/// テナント条件の lint（クエリがテナントを跨いで読み書きしないか）
fn lint_queries(p: &Project) -> Vec<serde_json::Value> {
    let mut out = vec![];
    for (n, q) in &p.queries {
        let s = q.sql.to_ascii_lowercase();
        let has_tenant = s.contains("tenant_id") || s.contains(":tenant");
        let upd = s.trim_start().starts_with("update") || s.trim_start().starts_with("delete");
        if !has_tenant {
            out.push(serde_json::json!({ "query": n, "level": if q.write { "error" } else { "warn" }, "msg": "テナント条件（tenant_id）がありません。他テナントのデータを読む / 更新する恐れがあります" }));
        }
        if upd && !s.contains(" where ") {
            out.push(serde_json::json!({ "query": n, "level": "error", "msg": "WHERE がない UPDATE / DELETE です（全行が対象になります）" }));
        }
        if s.contains("select *") {
            out.push(serde_json::json!({ "query": n, "level": "info", "msg": "SELECT * です。列を絞ると転送量と CPU が減ります" }));
        }
    }
    out
}
async fn lint_h(State(a): State<Arc<App>>) -> Json<Vec<serde_json::Value>> {
    Json(lint_queries(&a.project.read().unwrap()))
}

/// クエリごとの EXPLAIN（全件走査の警告）
async fn explain_h(State(a): State<Arc<App>>) -> Res<Json<Vec<serde_json::Value>>> {
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    let mut c = eng.connect().await.map_err(bad)?;
    let rows = schema::rows_per_tenant(&p);
    let mut out = vec![];
    for (n, q) in p.queries.iter().filter(|(_, q)| !q.write) {
        let (sql, _) = runner::render(&q.sql, 1, p.tenants, &rows, &Default::default(), None);
        let big: u64 = rows.iter().filter(|(t, _)| q.sql.contains(t.as_str())).map(|(_, r)| r * p.tenants as u64).max().unwrap_or(0);
        match eng.explain(c.as_mut(), &sql).await {
            Ok((scans, summary)) => out.push(serde_json::json!({
                "query": n, "full_scans": scans, "summary": summary, "table_rows": big,
                "warn": if scans > 0 && big > 2000 { format!("{big} 行の表を全件走査しています。インデックス（特に tenant_id を先頭にした複合）を確認してください") } else if scans > 0 { "全件走査ですが、表が小さいので問題ない可能性があります".to_string() } else { String::new() },
            })),
            Err(e) => out.push(serde_json::json!({ "query": n, "error": e.to_string() })),
        }
    }
    Ok(Json(out))
}

/// データ量と DB のメモリキャッシュの比較
async fn storage_h(State(a): State<Arc<App>>) -> Res<Json<serde_json::Value>> {
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    let mut c = eng.connect().await.map_err(bad)?;
    let (data, cache) = eng.storage_mb(c.as_mut()).await.map_err(bad)?;
    Ok(Json(serde_json::json!({ "data_mb": data, "cache_mb": cache, "fits": data <= cache * 0.8,
        "msg": if data > cache * 0.8 { "データがメモリキャッシュに収まりません。ディスク読み（IOPS）が出て、CPU 以外の限界が先に来る可能性があります" } else { "データはメモリキャッシュに収まっています（測定中はほぼメモリから読みます。本番でデータが増えると変わります）" } })))
}

#[derive(Deserialize)]
struct DdlReq {
    ddl: String,
    rows: Option<BTreeMap<String, u32>>,
    apply: Option<bool>,
}
async fn import_ddl_h(State(a): State<Arc<App>>, Json(r): Json<DdlReq>) -> Res<Json<serde_json::Value>> {
    let eng = a.project.read().unwrap().engine;
    let res = import::import_ddl(eng, &r.ddl, &r.rows.unwrap_or_default()).map_err(bad)?;
    if r.apply.unwrap_or(false) {
        let mut p = a.project.read().unwrap().clone();
        p.schema.tables = res.tables.clone();
        schema::validate(&p).map_err(bad)?;
        p.queries = schema::candidates(&p);
        p.actors.clear();
        *a.project.write().unwrap() = p;
        save(&a);
    }
    Ok(Json(serde_json::json!({ "tables": res.tables, "warnings": res.warnings, "applied": r.apply.unwrap_or(false) })))
}

#[derive(Deserialize)]
struct QImportReq {
    /// "text"（貼り付け）| "db"（pg_stat_statements / performance_schema）
    source: String,
    text: Option<String>,
    /// 統計の集計期間（秒）。回数 ÷ 期間 = 頻度
    window_secs: Option<f64>,
    top: Option<usize>,
    apply: Option<bool>,
}
async fn import_queries_h(State(a): State<Arc<App>>, Json(r): Json<QImportReq>) -> Res<Json<serde_json::Value>> {
    let p = a.project.read().unwrap().clone();
    let top = r.top.unwrap_or(20).clamp(1, 100);
    let mut window = r.window_secs.unwrap_or(0.0);
    let mut items: Vec<import::Imported> = if r.source == "text" {
        import::from_text(r.text.as_deref().unwrap_or(""))
    } else {
        let eng = engine_for(&p).map_err(bad)?;
        let mut c = eng.connect().await.map_err(bad)?;
        let sql = match p.engine {
            EngineKind::Postgres => format!("select query, calls from pg_stat_statements where dbid=(select oid from pg_database where datname=current_database()) order by calls desc limit {}", top * 3),
            EngineKind::Mysql => format!("select DIGEST_TEXT, COUNT_STAR from performance_schema.events_statements_summary_by_digest where DIGEST_TEXT is not null and SCHEMA_NAME = database() order by COUNT_STAR desc limit {}", top * 3),
        };
        if window <= 0.0 {
            // 統計の累積期間（MySQL: 稼働時間 / PostgreSQL: 統計のリセットからの経過）を既定にする
            let q = match p.engine {
                EngineKind::Postgres => "select extract(epoch from now() - stats_reset)::float8 from pg_stat_statements_info",
                EngineKind::Mysql => "select variable_value from performance_schema.global_status where variable_name='Uptime'",
            };
            window = c.rows(q).await.ok().and_then(|r| r.first().and_then(|x| x[0].parse::<f64>().ok())).unwrap_or(3600.0);
        }
        let rows = c.rows(&sql).await.map_err(|e| bad(format!("統計ビューを読めません: {e}（PostgreSQL は shared_preload_libraries に pg_stat_statements を入れて CREATE EXTENSION が必要。MySQL は performance_schema の有効化が必要）")))?;
        let text: String = rows.iter().map(|r| format!("-- calls={}\n{};\n", r[1], r[0].replace('`', ""))).collect();
        import::from_text(&text)
    };
    items.retain(|i| {
        let l = i.sql.to_ascii_lowercase();
        (l.starts_with("select") || l.starts_with("update") || l.starts_with("insert") || l.starts_with("delete"))
            && !l.contains("pg_stat") && !l.contains("pg_total_relation_size") && !l.contains("pg_size_bytes") && !l.contains("current_setting") && !l.contains("pg_terminate") && !l.contains("performance_schema") && !l.contains("information_schema") && !l.contains("pg_catalog") && !l.contains("pg_settings")
    });
    items.truncate(top);
    if window <= 0.0 {
        window = 3600.0;
    }
    if r.apply.unwrap_or(false) {
        let mut p = a.project.read().unwrap().clone();
        import::apply_imported(&mut p, &items, window);
        *a.project.write().unwrap() = p;
        save(&a);
    }
    Ok(Json(serde_json::json!({ "items": items, "window_secs": window, "applied": r.apply.unwrap_or(false),
        "note": "取り込んだクエリの値は 1〜1000 の乱数に置き換えます。表名は取り込み元のまま（このアプリのスキーマに同名の表が無いと実行できません）。書き込みは allow_writes を有効にしたときだけ実行します" })))
}

#[derive(Deserialize)]
struct MetricsImport {
    /// 例 "db.cpu_pct"（0〜100 の CPU 使用率）/ "db.cpu_util"（0〜1）/ 任意の名前（ext.<名前> として時系列に載る）
    name: String,
    /// `時刻,値` の行。時刻は実行開始からの秒 または UNIX 秒（CloudWatch / Datadog のエクスポートを貼れる）
    csv: String,
}
async fn import_metrics_h(State(a): State<Arc<App>>, Path(id): Path<String>, Json(r): Json<MetricsImport>) -> Res<Json<serde_json::Value>> {
    let run = a.runs.lock().unwrap().get(&id).cloned().ok_or((StatusCode::NOT_FOUND, "実行が見つかりません（このセッションで実行したものだけ取り込めます）".to_string()))?;
    let pts = import::parse_csv(&r.csv, run.started_unix).map_err(bad)?;
    let vcpu = run.project.profile.vcpu;
    let (warm, secs) = (run.opts.1 as f64, run.opts.0 as f64);
    let mut series = run.metrics.series.lock().unwrap();
    let mut n = 0;
    for (t, v) in &pts {
        // いちばん近い 1 秒の点に載せる（1.5 秒以内）
        if let Some(pt) = series.iter_mut().min_by(|x, y| (x.t - t).abs().partial_cmp(&(y.t - t).abs()).unwrap()) {
            if (pt.t - t).abs() <= 1.5 {
                pt.v.insert(format!("ext.{}", r.name), *v);
                if r.name == "db.cpu_pct" || r.name == "db.cpu_util" {
                    let util = if r.name == "db.cpu_pct" { v / 100.0 } else { *v };
                    pt.v.insert("db.cpu_util".into(), util);
                    pt.v.insert("db.cpu_cores".into(), util * vcpu);
                }
                n += 1;
            }
        }
    }
    // CPU を取り込んだときは、結論の CPU・安全上限を計算し直す
    if r.name == "db.cpu_pct" || r.name == "db.cpu_util" {
        let w: Vec<f64> = series.iter().filter(|x| x.t >= warm && x.t <= warm + secs).filter_map(|x| x.v.get("db.cpu_util").copied()).collect();
        if !w.is_empty() {
            let util = w.iter().sum::<f64>() / w.len() as f64;
            let cores = util * vcpu;
            if let Some(s) = run.summary.lock().unwrap().as_mut() {
                let pr = &run.project.profile;
                s.conclusion.cpu_util = Some(util);
                s.conclusion.cpu_cores_avg = Some(cores);
                let per_tenant = cores / run.project.tenants.max(1) as f64;
                if per_tenant > 0.0 {
                    s.conclusion.safe_tenants_extrapolated = Some(((pr.vcpu * pr.safe_util - pr.reserve_vcpu) / per_tenant).max(0.0));
                }
                s.conclusion.messages.push(format!("外部メトリクス（{}）から CPU 使用率 {:.0}% を取り込みました（{n} 点）", r.name, util * 100.0));
                if s.conclusion.bottleneck.starts_with("不明") {
                    s.conclusion.bottleneck = if util > 0.88 { "CPU（崩壊域）".into() } else if util > pr.safe_util { "CPU（安全枠超過）".into() } else { "余裕あり".into() };
                }
            }
        }
    }
    drop(series);
    if *run.state.lock().unwrap() == "finished" {
        let _ = std::fs::write(a.dir.join("runs").join(format!("{}.json", run.id)), serde_json::to_vec(&run_json(&run, true)).unwrap());
    }
    Ok(Json(serde_json::json!({ "attached_points": n, "name": r.name })))
}

async fn get_calib(State(a): State<Arc<App>>) -> Json<Calib> {
    Json(a.calib.read().unwrap().clone())
}

#[derive(Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
}
async fn doctor(State(a): State<Arc<App>>) -> Json<Vec<Check>> {
    let p = a.project.read().unwrap().clone();
    let mut v = vec![];
    let mut add = |n: &str, ok: bool, d: String| {
        v.push(Check {
            name: n.into(),
            ok,
            detail: d,
        })
    };
    match engine_for(&p) {
        Err(e) => add("接続先", false, e),
        Ok(eng) => match eng.connect().await {
            Err(e) => add("接続先", false, e.to_string()),
            Ok(mut c) => {
                add(
                    "接続先",
                    true,
                    format!("{:?} @ {}", p.engine, dsn_host(&eng.dsn)),
                );
                if let Ok(s) = eng.settings(c.as_mut()).await {
                    let durability = match p.engine {
                        EngineKind::Postgres => {
                            s.get("fsync").map(|x| x == "on").unwrap_or(true)
                                && s.get("synchronous_commit")
                                    .map(|x| x == "on")
                                    .unwrap_or(true)
                        }
                        EngineKind::Mysql => s
                            .get("innodb_flush_log_at_trx_commit")
                            .map(|x| x == "1")
                            .unwrap_or(true),
                    };
                    add(
                        "書き込みの耐久性設定",
                        durability,
                        format!(
                            "{s:?}{}",
                            if durability {
                                ""
                            } else {
                                " ← 耐久性が緩い設定です。書き込みを含む結果は実環境より速く出ます"
                            }
                        ),
                    );
                }
                let enc = eng.tls_in_use(c.as_mut()).await.unwrap_or(false);
                add("接続の暗号化", enc || p.tls == TlsMode::Off, format!("{}{}", if enc { "TLS で暗号化されています" } else { "暗号化していません" }, if !enc && !is_local(&eng.dsn) { "。リモートの DB（RDS / Aurora など）に平文で繋いでいます。設定の「接続の暗号化」を有効にしてください" } else { "" }));
                let src = cpu::CpuSrc::detect(is_local(&eng.dsn));
                let v1 = src.read();
                add(
                    "DB の CPU 採取",
                    v1.is_some(),
                    format!(
                        "{}{}",
                        src.name(),
                        if matches!(src, cpu::CpuSrc::Proc) {
                            "。正確に測るなら CAPLAB_CPU_CGROUP を設定"
                        } else {
                            ""
                        }
                    ),
                );
            }
        },
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    add("負荷側のコア数", cores >= 2, format!("{cores} コア。DB と同じホストで測るなら、負荷側と DB を別のコアに分けてください（taskset など）"));
    add(
        if p.allow_writes {
            "書き込み: 許可"
        } else {
            "書き込み: 禁止（既定）"
        },
        true,
        "書き込みクエリを実行するには、設定で「書き込みクエリの実行を許可する」を有効にします"
            .into(),
    );
    Json(v)
}

fn set_job(app: &App, f: impl FnOnce(&mut Job)) {
    f(&mut app.job.lock().unwrap());
}
fn job_busy(app: &App) -> bool {
    app.job.lock().unwrap().running
}

async fn schema_apply(State(a): State<Arc<App>>) -> Res<Json<Job>> {
    if job_busy(&a) {
        return Err(bad("別のジョブが実行中です"));
    }
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    schema::validate(&p).map_err(bad)?;
    set_job(&a, |j| {
        *j = Job {
            name: "スキーマ作成・データ投入".into(),
            step: "投入中".into(),
            running: true,
            ..Default::default()
        }
    });
    let app = a.clone();
    tokio::spawn(async move {
        let app2 = app.clone();
        let r =
            schema::apply_and_populate(&eng, &p, move |x| set_job(&app2, |j| j.progress = x)).await;
        set_job(&app, |j| {
            j.running = false;
            match r {
                Ok(n) => {
                    j.step = format!("完了（{n} 行）");
                    j.progress = 1.0;
                }
                Err(e) => j.error = Some(e.to_string()),
            }
        });
    });
    Ok(Json(a.job.lock().unwrap().clone()))
}

async fn do_calibrate(app: &Arc<App>, p: &Project, eng: &Engine, base: f64, span: f64) {
    let src = cpu::CpuSrc::detect(is_local(&eng.dsn));
    let app2 = app.clone();
    let c = runner::calibrate(p, eng, &src, 8, 2, Stop::new(), move |x, name| {
        set_job(&app2, |j| {
            j.step = format!("校正中: {name}");
            j.progress = base + span * x;
        })
    })
    .await;
    *app.calib.write().unwrap() = c;
    save(app);
}

async fn calibrate_h(State(a): State<Arc<App>>) -> Res<Json<Job>> {
    if job_busy(&a) {
        return Err(bad("別のジョブが実行中です"));
    }
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    set_job(&a, |j| {
        *j = Job {
            name: "校正".into(),
            running: true,
            ..Default::default()
        }
    });
    let app = a.clone();
    tokio::spawn(async move {
        do_calibrate(&app, &p, &eng, 0.0, 1.0).await;
        set_job(&app, |j| {
            j.running = false;
            j.step = "完了".into();
            j.progress = 1.0;
        });
    });
    Ok(Json(a.job.lock().unwrap().clone()))
}

#[derive(Deserialize)]
struct RunReq {
    secs: Option<u64>,
    warmup: Option<u64>,
}

async fn start_run(app: &Arc<App>, secs: u64, warmup: u64, over: Option<Project>) -> Result<String, String> {
    let p = over.unwrap_or_else(|| app.project.read().unwrap().clone());
    if p.actors.is_empty() {
        return Err("アクターがありません。「ざっくり設定」でワークロードを作ってください".into());
    }
    let eng = engine_for(&p)?;
    let mut conn = eng.connect().await.map_err(|e| e.to_string())?;
    let settings = eng.settings(conn.as_mut()).await.unwrap_or_default();
    let src = cpu::CpuSrc::detect(is_local(&eng.dsn));
    let mut notes = vec![];
    if let Ok((data, cache)) = eng.storage_mb(conn.as_mut()).await {
        if data > cache * 0.8 {
            notes.push(format!("データ {data:.0} MB が DB のメモリキャッシュ {cache:.0} MB に収まりません。ディスク読みが出て、CPU 以外が先に詰まる可能性があります"));
        }
    }
    // ネットワーク遅延の演出（中間サーバ）
    let mut sim: Option<net::NetSim> = None;
    let eng = if p.profile.net_mode == NetMode::Proxy && p.profile.rtt_ms > 0.0 {
        let (h, port) = net::host_port(p.engine, &eng.direct);
        let s = net::NetSim::start(format!("{h}:{port}"), p.profile.rtt_ms / 2.0, p.profile.jitter_ms).await.map_err(|e| e.to_string())?;
        let e2 = eng.with_dsn(net::rewrite_dsn(p.engine, &eng.direct, s.addr));
        match net::probe(&e2, 60).await {
            Ok(pr) => {
                let want = p.profile.rtt_ms;
                let off = (pr.rtt_p50_ms - want).abs() > (0.6f64).max(want * 0.3);
                notes.push(format!("遅延の演出（中間サーバ）: 設定 RTT {want:.2}ms / 計測 p50 {:.2}ms（接続確立 {:.1}ms）{}", pr.rtt_p50_ms, pr.connect_ms, if off { "。設定と計測が離れています（演出が不正確）" } else { "" }));
            }
            Err(e) => notes.push(format!("遅延の演出の確認に失敗: {e}")),
        }
        sim = Some(s);
        e2
    } else {
        eng
    };
    if p.actors.iter().any(|a| {
        a.steps
            .iter()
            .any(|s| p.queries.get(&s.query).map(|q| q.write).unwrap_or(false))
    }) {
        let loose = match p.engine {
            EngineKind::Postgres => {
                settings.get("fsync").map(|x| x != "on").unwrap_or(false)
                    || settings
                        .get("synchronous_commit")
                        .map(|x| x != "on")
                        .unwrap_or(false)
            }
            EngineKind::Mysql => settings
                .get("innodb_flush_log_at_trx_commit")
                .map(|x| x != "1")
                .unwrap_or(false),
        };
        if loose {
            notes.push("書き込みを含む負荷ですが、DB の耐久性設定が緩い（fsync / synchronous_commit / innodb_flush_log_at_trx_commit）ため、書き込みは実環境より速く出ます".into());
        }
    }
    // Reader（Aurora のリーダーエンドポイントなど）
    let reader = if !p.reader_dsn.is_empty() {
        let mut rp = p.clone();
        rp.dsn = p.reader_dsn.clone();
        Some(engine_for(&rp)?)
    } else {
        None
    };
    let id = format!("r{}", now_ms());
    let run = Arc::new(Run {
        id: id.clone(),
        project: p.clone(),
        metrics: Metrics::new(),
        stop: Stop::new(),
        events: Arc::new(Mutex::new(vec![])),
        summary: Mutex::new(None),
        state: Mutex::new("running".into()),
        settings: Mutex::new(settings),
        notes: Mutex::new(notes),
        cpu_src: src.name(),
        opts: (secs, warmup),
        started_unix: now_ms() as f64 / 1000.0,
        estimate: serde_json::to_value(estimate::estimate(
            &p,
            &app.calib.read().unwrap(),
            p.tenants,
        ))
        .unwrap(),
    });
    app.runs.lock().unwrap().insert(id.clone(), run.clone());
    let app2 = app.clone();
    tokio::spawn(async move {
        let _sim = sim; // 実行が終わるまで中間サーバを生かしておく
        let s = runner::execute(
            p,
            eng,
            reader,
            src,
            RunOpts {
                secs,
                warmup_secs: warmup,
            },
            run.metrics.clone(),
            run.stop.clone(),
            run.events.clone(),
        )
        .await;
        *run.summary.lock().unwrap() = Some(s);
        *run.state.lock().unwrap() = "finished".into();
        let _ = std::fs::create_dir_all(app2.dir.join("runs"));
        let _ = std::fs::write(
            app2.dir.join("runs").join(format!("{}.json", run.id)),
            serde_json::to_vec(&run_json(&run, true)).unwrap(),
        );
    });
    Ok(id)
}

async fn post_run(
    State(a): State<Arc<App>>,
    Json(r): Json<RunReq>,
) -> Res<Json<serde_json::Value>> {
    let id = start_run(
        &a,
        r.secs.unwrap_or(30).clamp(3, 7200),
        r.warmup.unwrap_or(5),
        None,
    )
    .await
    .map_err(bad)?;
    Ok(Json(serde_json::json!({ "id": id })))
}

/// かんたん実行: スキーマ作成・データ投入 → 校正 → 実行 を 1 つのジョブで通す
async fn easy(State(a): State<Arc<App>>, Json(r): Json<RunReq>) -> Res<Json<Job>> {
    if job_busy(&a) {
        return Err(bad("別のジョブが実行中です"));
    }
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    schema::validate(&p).map_err(bad)?;
    if p.actors.is_empty() {
        return Err(bad("先に「ざっくり設定」でワークロードを作ってください"));
    }
    let (secs, warm) = (r.secs.unwrap_or(30).clamp(3, 7200), r.warmup.unwrap_or(5));
    set_job(&a, |j| {
        *j = Job {
            name: "かんたん実行".into(),
            step: "1/3 スキーマ作成・データ投入".into(),
            running: true,
            ..Default::default()
        }
    });
    let app = a.clone();
    tokio::spawn(async move {
        let app2 = app.clone();
        if let Err(e) =
            schema::apply_and_populate(&eng, &p, move |x| set_job(&app2, |j| j.progress = x * 0.4))
                .await
        {
            return set_job(&app, |j| {
                j.running = false;
                j.error = Some(e.to_string());
            });
        }
        set_job(&app, |j| j.step = "2/3 校正（クエリ単体の CPU/回）".into());
        do_calibrate(&app, &p, &eng, 0.4, 0.2).await;
        set_job(&app, |j| {
            j.step = "3/3 負荷実行".into();
            j.progress = 0.6;
        });
        match start_run(&app, secs, warm, None).await {
            Err(e) => set_job(&app, |j| {
                j.running = false;
                j.error = Some(e);
            }),
            Ok(id) => {
                set_job(&app, |j| j.run_id = Some(id.clone()));
                let run = app.runs.lock().unwrap().get(&id).cloned();
                if let Some(run) = run {
                    while *run.state.lock().unwrap() == "running" {
                        let t = run.events.lock().unwrap().len();
                        let _ = t;
                        let el = run
                            .metrics
                            .series
                            .lock()
                            .unwrap()
                            .last()
                            .map(|x| x.t)
                            .unwrap_or(0.0);
                        set_job(&app, |j| {
                            j.progress = 0.6 + 0.4 * (el / (secs + warm) as f64).min(1.0)
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
                set_job(&app, |j| {
                    j.running = false;
                    j.step = "完了".into();
                    j.progress = 1.0;
                });
            }
        }
    });
    Ok(Json(a.job.lock().unwrap().clone()))
}

async fn get_job(State(a): State<Arc<App>>) -> Json<Job> {
    Json(a.job.lock().unwrap().clone())
}

/// 実行が終わるまで待って、要約を返す
async fn wait_run(app: &Arc<App>, id: &str) -> Option<(Summary, Project)> {
    let run = app.runs.lock().unwrap().get(id).cloned()?;
    while *run.state.lock().unwrap() == "running" {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let s = run.summary.lock().unwrap().clone()?;
    Some((s, run.project.clone()))
}

fn worst_p95(s: &Summary, p: &Project) -> f64 {
    s.actors.iter().filter(|a| p.actors.iter().find(|x| x.name == a.name).map(|x| x.kind != Kind::Batch).unwrap_or(true)).map(|a| a.p95_ms).fold(0.0, f64::max)
}

fn save_result(app: &App, id: &str, v: &serde_json::Value) {
    let _ = std::fs::create_dir_all(app.dir.join("results"));
    let _ = std::fs::write(app.dir.join("results").join(format!("{id}.json")), serde_json::to_vec(v).unwrap());
    app.results.lock().unwrap().insert(id.to_string(), v.clone());
}

#[derive(Deserialize)]
struct SweepReq {
    tenants: Vec<u32>,
    secs: Option<u64>,
    warmup: Option<u64>,
}

/// テナント数の掃引: データを最大数ぶん用意して、小さい順に実行し、限界（膝）を探す
async fn sweep_h(State(a): State<Arc<App>>, Json(r): Json<SweepReq>) -> Res<Json<Job>> {
    if job_busy(&a) {
        return Err(bad("別のジョブが実行中です"));
    }
    let mut ns = r.tenants.clone();
    ns.sort();
    ns.dedup();
    if ns.is_empty() || ns.len() > 30 {
        return Err(bad("テナント数は 1〜30 個の値で指定してください"));
    }
    let base = a.project.read().unwrap().clone();
    if base.actors.is_empty() {
        return Err(bad("先にワークロードを作ってください"));
    }
    let eng = engine_for(&base).map_err(bad)?;
    let (secs, warm) = (r.secs.unwrap_or(15).clamp(3, 600), r.warmup.unwrap_or(4));
    set_job(&a, |j| *j = Job { name: "テナント数の掃引".into(), step: "データ投入".into(), running: true, ..Default::default() });
    let app = a.clone();
    tokio::spawn(async move {
        let mut full = base.clone();
        full.tenants = *ns.last().unwrap();
        let app2 = app.clone();
        if let Err(e) = schema::apply_and_populate(&eng, &full, move |x| set_job(&app2, |j| j.progress = x * 0.3)).await {
            return set_job(&app, |j| { j.running = false; j.error = Some(e.to_string()); });
        }
        let id = format!("sweep{}", now_ms());
        let mut points = vec![];
        for (i, n) in ns.iter().enumerate() {
            set_job(&app, |j| { j.step = format!("{n} テナントを実行（{}/{}）", i + 1, ns.len()); j.progress = 0.3 + 0.7 * i as f64 / ns.len() as f64; });
            let mut p = base.clone();
            p.tenants = *n;
            p.chaos.clear(); // 掃引は限界を探る実行なので、障害注入は入れない
            let rid = match start_run(&app, secs, warm, Some(p)).await {
                Ok(x) => x,
                Err(e) => return set_job(&app, |j| { j.running = false; j.error = Some(e); }),
            };
            let Some((s, pr)) = wait_run(&app, &rid).await else { continue };
            let c = &s.conclusion;
            points.push(serde_json::json!({
                "tenants": n, "run_id": rid, "util": c.cpu_util, "cpu_cores": c.cpu_cores_avg,
                "worst_p95_ms": worst_p95(&s, &pr), "qps": s.queries.values().map(|v| v["qps"].as_f64().unwrap_or(0.0)).sum::<f64>(),
                "errors": s.actors.iter().map(|x| x.errors).sum::<u64>(), "dropped": s.actors.iter().map(|x| x.dropped).sum::<u64>(),
                "bottleneck": c.bottleneck, "slo_ok": c.slo_ok, "pool_wait_max": c.pool_wait_max,
            }));
        }
        // 負荷側が詰まった点は測定として信用できないので、膝・安全上限の判定から除く（その点以降は「負荷側の限界」として別に示す）
        let safe_util = base.profile.safe_util;
        let invalid = |p: &serde_json::Value| p["bottleneck"].as_str().map(|b| b.starts_with("負荷")).unwrap_or(false);
        let bad_pt = |p: &serde_json::Value| !p["slo_ok"].as_bool().unwrap_or(false) || p["util"].as_f64().map(|u| u > 0.88).unwrap_or(false);
        let generator_limit = points.iter().find(|p| invalid(p)).and_then(|p| p["tenants"].as_u64());
        let valid: Vec<&serde_json::Value> = points.iter().filter(|p| !invalid(p)).collect();
        let knee = valid.iter().find(|p| bad_pt(p)).and_then(|p| p["tenants"].as_u64());
        let safe = valid.iter().take_while(|p| !bad_pt(p)).filter(|p| p["util"].as_f64().map(|u| u <= safe_util).unwrap_or(true)).filter_map(|p| p["tenants"].as_u64()).last();
        let res = serde_json::json!({ "id": id, "kind": "sweep", "points": points, "knee": knee, "generator_limit": generator_limit, "safe_tenants": safe, "safe_util": safe_util, "secs": secs });
        save_result(&app, &id, &res);
        set_job(&app, |j| { j.running = false; j.step = "完了".into(); j.progress = 1.0; });
    });
    Ok(Json(a.job.lock().unwrap().clone()))
}

#[derive(Deserialize)]
struct RepeatReq {
    n: u32,
    secs: Option<u64>,
    warmup: Option<u64>,
}

/// N 回繰り返して、平均とばらつきを出す（1 回だけの測定を信用しないため）
async fn repeat_h(State(a): State<Arc<App>>, Json(r): Json<RepeatReq>) -> Res<Json<Job>> {
    if job_busy(&a) {
        return Err(bad("別のジョブが実行中です"));
    }
    let n = r.n.clamp(2, 20);
    let p = a.project.read().unwrap().clone();
    if p.actors.is_empty() {
        return Err(bad("先にワークロードを作ってください"));
    }
    engine_for(&p).map_err(bad)?;
    let (secs, warm) = (r.secs.unwrap_or(15).clamp(3, 600), r.warmup.unwrap_or(4));
    set_job(&a, |j| *j = Job { name: format!("{n} 回の繰り返し"), step: "開始".into(), running: true, ..Default::default() });
    let app = a.clone();
    tokio::spawn(async move {
        let id = format!("repeat{}", now_ms());
        let mut runs = vec![];
        let mut series: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        for i in 0..n {
            set_job(&app, |j| { j.step = format!("{}/{} 回目", i + 1, n); j.progress = i as f64 / n as f64; });
            let rid = match start_run(&app, secs, warm, None).await {
                Ok(x) => x,
                Err(e) => return set_job(&app, |j| { j.running = false; j.error = Some(e); }),
            };
            let Some((s, pr)) = wait_run(&app, &rid).await else { continue };
            let c = &s.conclusion;
            series.entry("worst_p95_ms").or_default().push(worst_p95(&s, &pr));
            if let Some(x) = c.cpu_cores_avg { series.entry("cpu_cores").or_default().push(x); }
            series.entry("qps").or_default().push(s.queries.values().map(|v| v["qps"].as_f64().unwrap_or(0.0)).sum());
            series.entry("errors").or_default().push(s.actors.iter().map(|x| x.errors).sum::<u64>() as f64);
            runs.push(rid);
        }
        let mut stats = serde_json::Map::new();
        for (k, v) in &series {
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let sd = if v.len() > 1 { (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt() } else { 0.0 };
            stats.insert(k.to_string(), serde_json::json!({ "mean": mean, "stdev": sd, "cv_pct": if mean > 0.0 { sd / mean * 100.0 } else { 0.0 }, "min": v.iter().cloned().fold(f64::MAX, f64::min), "max": v.iter().cloned().fold(0.0, f64::max), "values": v }));
        }
        let res = serde_json::json!({ "id": id, "kind": "repeat", "n": n, "runs": runs, "stats": stats, "secs": secs });
        save_result(&app, &id, &res);
        set_job(&app, |j| { j.running = false; j.step = "完了".into(); j.progress = 1.0; });
    });
    Ok(Json(a.job.lock().unwrap().clone()))
}

async fn list_results(State(a): State<Arc<App>>) -> Json<Vec<serde_json::Value>> {
    let mut out: Vec<serde_json::Value> = a.results.lock().unwrap().values().cloned().collect();
    if let Ok(rd) = std::fs::read_dir(a.dir.join("results")) {
        for e in rd.flatten() {
            if let Ok(v) = std::fs::read(e.path()).map_err(|_| ()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).map_err(|_| ())) {
                if !out.iter().any(|x| x["id"] == v["id"]) { out.push(v); }
            }
        }
    }
    // id は `sweep<ms>` / `repeat<ms>`。種類ではなく時刻（数字部分）の新しい順に並べる
    let ts = |v: &serde_json::Value| v["id"].as_str().unwrap_or("").trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u128>().unwrap_or(0);
    out.sort_by(|a, b| ts(b).cmp(&ts(a)));
    Json(out)
}

fn run_json(r: &Run, with_series: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": r.id, "state": *r.state.lock().unwrap(), "opts": {"secs": r.opts.0, "warmup": r.opts.1},
        "tenants": r.project.tenants, "engine": r.project.engine, "vcpu": r.project.profile.vcpu,
        "cpu_source": r.cpu_src, "settings": *r.settings.lock().unwrap(), "notes": *r.notes.lock().unwrap(),
        "events": *r.events.lock().unwrap(), "summary": *r.summary.lock().unwrap(),
        "estimate": r.estimate, "actors": r.project.actors.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
    });
    if with_series {
        v["series"] = serde_json::to_value(&*r.metrics.series.lock().unwrap()).unwrap();
    }
    v
}
async fn list_runs(State(a): State<Arc<App>>) -> Json<Vec<serde_json::Value>> {
    let mut out: Vec<serde_json::Value> = a
        .runs
        .lock()
        .unwrap()
        .values()
        .map(|r| run_json(r, false))
        .collect();
    if let Ok(rd) = std::fs::read_dir(a.dir.join("runs")) {
        for e in rd.flatten() {
            if let Ok(b) = std::fs::read(e.path()) {
                if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    if out.iter().any(|x| x["id"] == v["id"]) {
                        continue;
                    }
                    v.as_object_mut().map(|o| o.remove("series"));
                    out.push(v);
                }
            }
        }
    }
    out.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    Json(out)
}
#[derive(Deserialize)]
struct RunQ {
    series: Option<u8>,
}
async fn get_run(
    State(a): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<RunQ>,
) -> Res<Json<serde_json::Value>> {
    if let Some(r) = a.runs.lock().unwrap().get(&id) {
        return Ok(Json(run_json(r, q.series.unwrap_or(1) != 0)));
    }
    let b = std::fs::read(a.dir.join("runs").join(format!("{id}.json")))
        .map_err(|_| (StatusCode::NOT_FOUND, "実行が見つかりません".into()))?;
    Ok(Json(serde_json::from_slice(&b).map_err(bad)?))
}
async fn stop_run(State(a): State<Arc<App>>, Path(id): Path<String>) -> impl IntoResponse {
    if let Some(r) = a.runs.lock().unwrap().get(&id) {
        r.stop.set();
        return StatusCode::OK;
    }
    StatusCode::NOT_FOUND
}
async fn stream_run(
    State(a): State<Arc<App>>,
    Path(id): Path<String>,
) -> Res<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>> {
    let r = a
        .runs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "実行が見つかりません".to_string()))?;
    let existing = r.metrics.series.lock().unwrap().clone();
    let rx = r.metrics.tx.subscribe();
    let first = futures::stream::iter(
        existing
            .into_iter()
            .map(|p| Ok(Event::default().data(serde_json::to_string(&p).unwrap()))),
    );
    let live = BroadcastStream::new(rx).filter_map(|x| async move {
        x.ok()
            .map(|p| Ok(Event::default().data(serde_json::to_string(&p).unwrap())))
    });
    Ok(Sse::new(first.chain(live)).keep_alive(axum::response::sse::KeepAlive::default()))
}

#[derive(Deserialize)]
struct ProbeReq {
    n: Option<usize>,
    gap_ms: Option<u64>,
}
async fn probe_h(State(a): State<Arc<App>>, Json(r): Json<ProbeReq>) -> Res<Json<serde_json::Value>> {
    let p = a.project.read().unwrap().clone();
    let eng = engine_for(&p).map_err(bad)?;
    let n = r.n.unwrap_or(100).clamp(10, 2000);
    let direct = net::probe_gap(&eng, n, r.gap_ms.unwrap_or(0)).await.map_err(bad)?;
    let mut out = serde_json::json!({ "direct": direct });
    if p.profile.rtt_ms > 0.0 {
        let (h, port) = net::host_port(p.engine, &eng.direct);
        let s = net::NetSim::start(format!("{h}:{port}"), p.profile.rtt_ms / 2.0, p.profile.jitter_ms).await.map_err(bad)?;
        let sim = net::probe(&eng.with_dsn(net::rewrite_dsn(p.engine, &eng.direct, s.addr)), n).await.map_err(bad)?;
        out["simulated"] = serde_json::json!({ "configured_rtt_ms": p.profile.rtt_ms, "measured": sim });
    }
    Ok(Json(out))
}
/// 実測した RTT をプロジェクトに保存する（試算に使う）
async fn probe_save(State(a): State<Arc<App>>, Json(v): Json<serde_json::Value>) -> Res<Json<Project>> {
    let rtt = v["rtt_ms"].as_f64().ok_or_else(|| bad("rtt_ms がありません"))?;
    a.project.write().unwrap().profile.measured_rtt_ms = if rtt > 0.0 { Some(rtt) } else { None };
    save(&a);
    Ok(Json(a.project.read().unwrap().clone()))
}

fn default_project() -> Project {
    let t = |n: &str, par: Option<&str>, cols, size, rows| TableSpec {
        name: n.into(),
        parent: par.map(|s| s.into()),
        columns: cols,
        size,
        rows,
    };
    Project {
        name: "sample".into(),
        tenants: 100,
        schema: Schema {
            tables: vec![
                t("sites", None, 4, Size::M, 20),
                t("devices", Some("sites"), 5, Size::S, 5),
                t("readings", Some("devices"), 4, Size::S, 10),
            ],
            fk_index: true,
            auth_users_per_tenant: Some(50),
        },
        ..Default::default()
    }
}

#[tokio::main]
async fn main() {
    // TLS の暗号プロバイダを明示する（mysql_async と tokio-postgres-rustls が別々に rustls を使うため、自動選択に任せるとパニックする）
    let _ = rustls::crypto::ring::default_provider().install_default();
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return probe_cli().await;
    }
    let dir = PathBuf::from(std::env::var("CAPLAB_DIR").unwrap_or_else(|_| "caplab-data".into()));
    let project = std::fs::read(dir.join("project.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(default_project);
    let calib = std::fs::read(dir.join("calib.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let app = Arc::new(App {
        dir,
        project: RwLock::new(project),
        calib: RwLock::new(calib),
        runs: Mutex::new(BTreeMap::new()),
        job: Mutex::new(Job::default()),
        results: Mutex::new(BTreeMap::new()),
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/api/project", get(get_project).put(put_project))
        .route("/api/simple", post(post_simple))
        .route("/api/candidates", get(get_candidates))
        .route("/api/ddl", get(get_ddl))
        .route("/api/estimate", get(get_estimate))
        .route("/api/calib", get(get_calib))
        .route("/api/doctor", get(doctor))
        .route("/api/import/ddl", post(import_ddl_h))
        .route("/api/import/queries", post(import_queries_h))
        .route("/api/runs/:id/import_metrics", post(import_metrics_h))
        .route("/api/lint", get(lint_h))
        .route("/api/explain", get(explain_h))
        .route("/api/storage", get(storage_h))
        .route("/api/sweep", post(sweep_h))
        .route("/api/repeat", post(repeat_h))
        .route("/api/results", get(list_results))
        .route("/api/probe", post(probe_h))
        .route("/api/probe/save", post(probe_save))
        .route("/api/schema/apply", post(schema_apply))
        .route("/api/calibrate", post(calibrate_h))
        .route("/api/easy", post(easy))
        .route("/api/job", get(get_job))
        .route("/api/runs", get(list_runs).post(post_run))
        .route("/api/runs/:id", get(get_run))
        .route("/api/runs/:id/stop", post(stop_run))
        .route("/api/runs/:id/stream", get(stream_run))
        .with_state(app);
    let addr = std::env::var("CAPLAB_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    println!("caplab: http://{addr}");
    let l = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(l, router).await.unwrap();
}

/// `caplab probe [--save]`: ECS のコンテナ内などで実行し、接続確立の時間と SELECT 1 の往復を測る
async fn probe_cli() {
    let dir = PathBuf::from(std::env::var("CAPLAB_DIR").unwrap_or_else(|_| "caplab-data".into()));
    let mut p: Project = std::fs::read(dir.join("project.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(default_project);
    if let Ok(e) = std::env::var("CAPLAB_ENGINE") {
        p.engine = if e == "mysql" { EngineKind::Mysql } else { EngineKind::Postgres };
    }
    let eng = match engine_for(&p) {
        Ok(e) => e,
        Err(e) => return eprintln!("{e}"),
    };
    match net::probe(&eng, 500).await {
        Ok(r) => {
            println!("接続確立 p50 {:.2} ms ／ SELECT 1 の往復 p50 {:.3} ms, p95 {:.3} ms, min {:.3} ms（{} 回）", r.connect_ms, r.rtt_p50_ms, r.rtt_p95_ms, r.rtt_min_ms, r.samples);
            if std::env::args().any(|a| a == "--save") {
                p.profile.measured_rtt_ms = Some(r.rtt_p50_ms);
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join("project.json"), serde_json::to_vec_pretty(&p).unwrap());
                println!("プロジェクトの measured_rtt_ms に保存しました");
            }
        }
        Err(e) => eprintln!("probe 失敗: {e}"),
    }
}
