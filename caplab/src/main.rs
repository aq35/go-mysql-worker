mod cpu;
mod engine;
mod estimate;
mod model;
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
}

struct App {
    dir: PathBuf,
    project: RwLock<Project>,
    calib: RwLock<Calib>,
    runs: Mutex<BTreeMap<String, Arc<Run>>>,
    job: Mutex<Job>,
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
    Ok(Engine::new(p.engine, dsn))
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
    }
    let c = a.calib.read().unwrap().clone();
    Json(estimate::estimate(&p, &c, q.tenants.unwrap_or(p.tenants)))
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

async fn start_run(app: &Arc<App>, secs: u64, warmup: u64) -> Result<String, String> {
    let p = app.project.read().unwrap().clone();
    if p.actors.is_empty() {
        return Err("アクターがありません。「ざっくり設定」でワークロードを作ってください".into());
    }
    let eng = engine_for(&p)?;
    let mut conn = eng.connect().await.map_err(|e| e.to_string())?;
    let settings = eng.settings(conn.as_mut()).await.unwrap_or_default();
    let src = cpu::CpuSrc::detect(is_local(&eng.dsn));
    let mut notes = vec![];
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
        let s = runner::execute(
            p,
            eng,
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
        match start_run(&app, secs, warm).await {
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
