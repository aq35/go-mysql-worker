//! かんたんスキーマ: 親→子→孫の表の DDL・テストデータ・クエリ候補・ワークロードのひな形を作る。
use crate::engine::Engine;
use crate::model::*;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;

pub const SESSIONS: &str = "sessions";

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Big,
    Ts,
    Num,
    Str,
}

fn col_type(i: u32, columns: u32) -> Ty {
    if i + 1 == columns {
        return Ty::Str; // 少なくとも 1 つは文字列にしてサイズを調整できるようにする
    }
    [Ty::Int, Ty::Big, Ty::Str, Ty::Ts, Ty::Num, Ty::Str][(i % 6) as usize]
}

fn fixed_bytes(t: Ty) -> usize {
    match t {
        Ty::Int => 4,
        Ty::Big | Ty::Ts | Ty::Num => 8,
        Ty::Str => 0,
    }
}

/// 文字列カラム 1 つあたりの長さ（行サイズの目標から逆算）
fn str_len(t: &TableSpec) -> usize {
    let mut fixed = 12; // tenant_id, id, parent_id
    let mut n_str = 0;
    for i in 0..t.columns {
        match col_type(i, t.columns) {
            Ty::Str => n_str += 1,
            ty => fixed += fixed_bytes(ty),
        }
    }
    if n_str == 0 {
        return 0;
    }
    (t.size.row_bytes().saturating_sub(fixed) / n_str).max(1)
}

pub fn depth_of(schema: &Schema, name: &str) -> usize {
    let mut d = 0;
    let mut cur = schema.tables.iter().find(|t| t.name == name);
    while let Some(t) = cur {
        d += 1;
        cur = t
            .parent
            .as_ref()
            .and_then(|p| schema.tables.iter().find(|x| &x.name == p));
    }
    d
}

/// 1 テナントあたりの各表の件数
pub fn rows_per_tenant(p: &Project) -> BTreeMap<String, u64> {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    let mut tables: Vec<&TableSpec> = p.schema.tables.iter().collect();
    tables.sort_by_key(|t| depth_of(&p.schema, &t.name));
    for t in tables {
        let n = match &t.parent {
            None => t.rows as u64,
            Some(par) => m.get(par).copied().unwrap_or(1) * t.rows as u64,
        };
        m.insert(t.name.clone(), n);
    }
    if let Some(u) = p.schema.auth_users_per_tenant {
        m.insert(SESSIONS.into(), u as u64);
    }
    m
}

pub fn validate(p: &Project) -> Result<()> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !s.chars().next().unwrap().is_ascii_digit()
    };
    for t in &p.schema.tables {
        if !ident(&t.name) {
            return Err(anyhow!(
                "表名 '{}' は英小文字・数字・_ のみ（先頭は英字）",
                t.name
            ));
        }
        if t.name == SESSIONS {
            return Err(anyhow!("'{SESSIONS}' は認証用に予約されています"));
        }
        if let Some(par) = &t.parent {
            if !p.schema.tables.iter().any(|x| &x.name == par) {
                return Err(anyhow!("表 '{}' の親 '{}' がありません", t.name, par));
            }
        }
        if t.rows == 0 || t.columns > 60 {
            return Err(anyhow!("表 '{}' の件数 / カラム数が不正です", t.name));
        }
    }
    Ok(())
}

fn ty_sql(kind: EngineKind, t: Ty, len: usize) -> String {
    match (kind, t) {
        (_, Ty::Int) => "INTEGER".into(),
        (_, Ty::Big) => "BIGINT".into(),
        (EngineKind::Postgres, Ty::Ts) => "TIMESTAMP".into(),
        (EngineKind::Mysql, Ty::Ts) => "DATETIME".into(),
        (EngineKind::Postgres, Ty::Num) => "NUMERIC(12,2)".into(),
        (EngineKind::Mysql, Ty::Num) => "DECIMAL(12,2)".into(),
        (_, Ty::Str) => {
            if len <= 255 {
                format!("VARCHAR({})", len.max(1))
            } else {
                "TEXT".into()
            }
        }
    }
}

pub fn ddl(p: &Project) -> Vec<String> {
    let kind = p.engine;
    let mut out = vec![];
    for t in &p.schema.tables {
        let len = str_len(t);
        let mut cols = vec![
            "tenant_id INTEGER NOT NULL".to_string(),
            "id INTEGER NOT NULL".to_string(),
        ];
        if t.parent.is_some() {
            cols.push("parent_id INTEGER NOT NULL".into());
        }
        for i in 0..t.columns {
            cols.push(format!(
                "c{} {}",
                i + 1,
                ty_sql(kind, col_type(i, t.columns), len)
            ));
        }
        out.push(format!("DROP TABLE IF EXISTS {}", t.name));
        out.push(format!(
            "CREATE TABLE {} ({}, PRIMARY KEY (tenant_id, id))",
            t.name,
            cols.join(", ")
        ));
        if t.parent.is_some() && p.schema.fk_index {
            out.push(format!(
                "CREATE INDEX {}_parent ON {} (tenant_id, parent_id)",
                t.name, t.name
            ));
        }
    }
    if p.schema.auth_users_per_tenant.is_some() {
        let ts = ty_sql(kind, Ty::Ts, 0);
        out.push(format!("DROP TABLE IF EXISTS {SESSIONS}"));
        out.push(format!("CREATE TABLE {SESSIONS} (tenant_id INTEGER NOT NULL, id INTEGER NOT NULL, user_id INTEGER NOT NULL, expires_at {ts} NOT NULL, PRIMARY KEY (tenant_id, id))"));
    }
    out
}

/// 1 テナント分の INSERT 文（バッチに分割）。値の中身は決めず、サイズだけを合わせる。
fn insert_stmts(p: &Project, t: &TableSpec, tenant: u32, rows: u64) -> Vec<String> {
    let len = str_len(t);
    let payload: String = "x".repeat(len);
    let row_bytes = t.size.row_bytes().max(50);
    let batch = (1_000_000 / row_bytes).clamp(1, 2000) as u64;
    let per_parent = t.rows as u64;
    let mut out = vec![];
    let mut id = 1u64;
    while id <= rows {
        let end = (id + batch - 1).min(rows);
        let mut vals = Vec::with_capacity((end - id + 1) as usize);
        for i in id..=end {
            let mut v = format!("({tenant},{i}");
            if t.parent.is_some() {
                v.push_str(&format!(",{}", (i - 1) / per_parent + 1));
            }
            for c in 0..t.columns {
                match col_type(c, t.columns) {
                    Ty::Int => v.push_str(&format!(",{}", i % 100)),
                    Ty::Big => v.push_str(&format!(",{}", i * 7)),
                    Ty::Ts => v.push_str(",'2026-01-01 00:00:00'"),
                    Ty::Num => v.push_str(&format!(",{}.25", i % 997)),
                    Ty::Str => {
                        v.push_str(",'");
                        v.push_str(&payload);
                        v.push('\'');
                    }
                }
            }
            v.push(')');
            vals.push(v);
        }
        let mut cols = "tenant_id, id".to_string();
        if t.parent.is_some() {
            cols.push_str(", parent_id");
        }
        for c in 0..t.columns {
            cols.push_str(&format!(", c{}", c + 1));
        }
        out.push(format!(
            "INSERT INTO {} ({}) VALUES {}",
            t.name,
            cols,
            vals.join(",")
        ));
        id = end + 1;
    }
    let _ = p;
    out
}

fn session_stmts(tenant: u32, users: u32) -> Vec<String> {
    let vals: Vec<String> = (1..=users)
        .map(|i| format!("({tenant},{i},{i},'2099-01-01 00:00:00')"))
        .collect();
    vals.chunks(1000)
        .map(|c| {
            format!(
                "INSERT INTO {SESSIONS} (tenant_id, id, user_id, expires_at) VALUES {}",
                c.join(",")
            )
        })
        .collect()
}

/// DDL を適用し、全テナント分のデータを投入する。progress は 0.0..1.0 を通知。
pub async fn apply_and_populate(
    engine: &Engine,
    p: &Project,
    progress: impl Fn(f64) + Send + Sync + 'static,
) -> Result<u64> {
    validate(p)?;
    let mut c = engine.connect().await?;
    for s in ddl(p) {
        c.exec(&s)
            .await
            .map_err(|e| anyhow!("DDL 失敗: {s}: {e}"))?;
    }
    let rows = rows_per_tenant(p);
    let total_work = (p.tenants as u64).max(1);
    let workers = 4u32.min(p.tenants.max(1));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let progress = std::sync::Arc::new(progress);
    let mut handles = vec![];
    for w in 0..workers {
        let engine = engine.clone();
        let p = p.clone();
        let rows = rows.clone();
        let done = done.clone();
        let progress = progress.clone();
        handles.push(tokio::spawn(async move {
            let mut c = engine.connect().await?;
            let mut inserted = 0u64;
            let mut tenant = w + 1;
            while tenant <= p.tenants {
                for t in &p.schema.tables {
                    for s in insert_stmts(&p, t, tenant, rows[&t.name]) {
                        c.exec(&s)
                            .await
                            .map_err(|e| anyhow!("INSERT 失敗({}): {e}", t.name))?;
                    }
                    inserted += rows[&t.name];
                }
                if let Some(u) = p.schema.auth_users_per_tenant {
                    for s in session_stmts(tenant, u) {
                        c.exec(&s).await?;
                    }
                    inserted += u as u64;
                }
                let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                progress(d as f64 / total_work as f64);
                tenant += workers;
            }
            Ok::<u64, anyhow::Error>(inserted)
        }));
    }
    let mut n = 0;
    for h in handles {
        n += h.await??;
    }
    // 統計の更新（実行計画が現実に近くなるように）
    for t in &p.schema.tables {
        let _ = c
            .exec(&match p.engine {
                EngineKind::Postgres => format!("ANALYZE {}", t.name),
                EngineKind::Mysql => format!("ANALYZE TABLE {}", t.name),
            })
            .await;
    }
    Ok(n)
}

fn first_int_col(t: &TableSpec) -> Option<u32> {
    (0..t.columns)
        .find(|&i| matches!(col_type(i, t.columns), Ty::Int | Ty::Big))
        .map(|i| i + 1)
}

/// DDL 由来のクエリ候補（実際のアプリのクエリではない。候補として並べる）
pub fn candidates(p: &Project) -> BTreeMap<String, QueryDef> {
    let mut q = BTreeMap::new();
    let col = |t: &TableSpec| if t.columns > 0 { "c1" } else { "id" }.to_string();
    for t in &p.schema.tables {
        q.insert(
            format!("{}_pk", t.name),
            QueryDef {
                sql: format!(
                    "SELECT id, {} FROM {} WHERE tenant_id = :tenant AND id = :rand_{}",
                    col(t),
                    t.name,
                    t.name
                ),
                write: false,
            },
        );
        if let Some(par) = &t.parent {
            q.insert(
                format!("{}_by_{}", t.name, par),
                QueryDef {
                    sql: format!(
                        "SELECT id, {} FROM {} WHERE tenant_id = :tenant AND parent_id = :{}",
                        col(t),
                        t.name,
                        par
                    ),
                    write: false,
                },
            );
            q.insert(format!("{}_join_{}", t.name, par), QueryDef { sql: format!("SELECT c.id, c.{cc} FROM {par} p JOIN {child} c ON c.tenant_id = p.tenant_id AND c.parent_id = p.id WHERE p.tenant_id = :tenant AND p.id = :rand_{par}", cc = col(t), child = t.name, par = par), write: false });
        }
        if let Some(ci) = first_int_col(t) {
            q.insert(format!("{}_touch", t.name), QueryDef { sql: format!("UPDATE {tn} SET c{ci} = c{ci} + 1 WHERE tenant_id = :tenant AND id = :rand_{tn}", tn = t.name), write: true });
        }
    }
    // 孫まで一括（JOIN / IN）
    if let Some(g) = p
        .schema
        .tables
        .iter()
        .find(|t| depth_of(&p.schema, &t.name) == 3)
    {
        let m = g.parent.clone().unwrap();
        let r = p
            .schema
            .tables
            .iter()
            .find(|t| t.name == m)
            .and_then(|t| t.parent.clone())
            .unwrap();
        q.insert("tree_join".into(), QueryDef { sql: format!("SELECT g.id, g.{gc} FROM {r} r JOIN {m} m ON m.tenant_id = r.tenant_id AND m.parent_id = r.id JOIN {g} g ON g.tenant_id = m.tenant_id AND g.parent_id = m.id WHERE r.tenant_id = :tenant AND r.id = :rand_{r}", gc = col(g), g = g.name), write: false });
        q.insert("tree_in".into(), QueryDef { sql: format!("SELECT id, {gc} FROM {g} WHERE tenant_id = :tenant AND parent_id IN (SELECT id FROM {m} WHERE tenant_id = :tenant AND parent_id = :{r})", gc = col(g), g = g.name), write: false });
    }
    if p.schema.auth_users_per_tenant.is_some() {
        q.insert("auth_session".into(), QueryDef { sql: format!("SELECT user_id FROM {SESSIONS} WHERE tenant_id = :tenant AND id = :rand_{SESSIONS} AND expires_at > CURRENT_TIMESTAMP"), write: false });
    }
    // 重いバッチ: 複数テナント分をまとめて集計
    if let Some(deep) = p
        .schema
        .tables
        .iter()
        .max_by_key(|t| depth_of(&p.schema, &t.name))
    {
        let sql = match &deep.parent {
            Some(par) => format!("SELECT count(*), max(c.id) FROM {d} c JOIN {par} p ON p.tenant_id = c.tenant_id AND p.id = c.parent_id WHERE c.tenant_id BETWEEN :tenant AND :tenant_hi", d = deep.name, par = par),
            None => format!("SELECT count(*), max(id) FROM {} WHERE tenant_id BETWEEN :tenant AND :tenant_hi", deep.name),
        };
        q.insert("batch_agg".into(), QueryDef { sql, write: false });
    }
    q
}

/// 木の経路（ルート → … → 最深）
fn chain(p: &Project) -> Vec<String> {
    let Some(deep) = p
        .schema
        .tables
        .iter()
        .max_by_key(|t| depth_of(&p.schema, &t.name))
    else {
        return vec![];
    };
    let mut c = vec![deep.name.clone()];
    let mut cur = deep;
    while let Some(par) = &cur.parent {
        c.push(par.clone());
        cur = p.schema.tables.iter().find(|t| &t.name == par).unwrap();
    }
    c.reverse();
    c
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct Simple {
    pub tenants: u32,
    /// 1 テナントあたりの Web ユーザー数
    pub users_per_tenant: u32,
    /// 1 ユーザーあたりの操作（回/分）
    pub user_ops_per_min: f64,
    /// ワーカー（間隔 ms のリスト。例 [1000, 10000, 10000, 60000, 60000]）
    pub worker_every_ms: Vec<u64>,
    /// "nplus1" | "join" | "in"
    pub web_flow: String,
    pub auth_cache_hit: f64,
    pub batch: bool,
    pub stagger: bool,
}

fn flow_steps(p: &Project, flow: &str) -> Vec<Step> {
    let ch = chain(p);
    let st = |id: Option<&str>, q: String| Step {
        id: id.map(|s| s.into()),
        query: q,
        foreach: None,
        limit: None,
        repeat: None,
        cache_hit: None,
    };
    match (flow, ch.len()) {
        (_, 0) => vec![],
        (_, 1) => vec![st(Some(&ch[0]), format!("{}_pk", ch[0]))],
        ("join", 2) => vec![st(None, format!("{}_join_{}", ch[1], ch[0]))],
        ("join", _) => vec![st(None, "tree_join".into())],
        ("in", 3) => vec![
            st(Some(&ch[0]), format!("{}_pk", ch[0])),
            st(None, "tree_in".into()),
        ],
        (_, 2) => vec![
            st(Some(&ch[0]), format!("{}_pk", ch[0])),
            st(None, format!("{}_by_{}", ch[1], ch[0])),
        ],
        (_, _) => {
            let mut v = vec![
                st(Some(&ch[0]), format!("{}_pk", ch[0])),
                st(Some(&ch[1]), format!("{}_by_{}", ch[1], ch[0])),
            ];
            let mut g = st(None, format!("{}_by_{}", ch[2], ch[1]));
            g.foreach = Some(ch[1].clone());
            g.limit = Some(20);
            v.push(g);
            v
        }
    }
}

/// 「ざっくりモード」: 入力が少ない仮のワークロードを作る。プロジェクトの queries / actors / tenants を上書きする。
pub fn apply_simple(p: &mut Project, s: &Simple) {
    p.tenants = s.tenants;
    p.schema.auth_users_per_tenant = Some(s.users_per_tenant.max(1));
    p.queries = candidates(p);
    p.actors.clear();
    for (i, every) in s.worker_every_ms.iter().enumerate() {
        p.actors.push(Actor {
            name: format!("worker-{}ms-{}", every, i + 1),
            kind: Kind::Worker,
            every_ms: Some(*every),
            instances: 1,
            qps_per_tenant: None,
            stagger: s.stagger,
            concurrency: None,
            tenant_limit: None,
            pool_max_open: 10,
            tx: false,
            steps: flow_steps(p, "nplus1"),
        });
    }
    let qps = s.users_per_tenant as f64 * s.user_ops_per_min / 60.0;
    if qps > 0.0 {
        let mut steps = vec![Step {
            id: None,
            query: "auth_session".into(),
            foreach: None,
            limit: None,
            repeat: None,
            cache_hit: Some(s.auth_cache_hit),
        }];
        steps.extend(flow_steps(p, &s.web_flow));
        p.actors.push(Actor {
            name: "web-api".into(),
            kind: Kind::Web,
            every_ms: None,
            instances: 1,
            qps_per_tenant: Some(qps),
            stagger: false,
            concurrency: None,
            tenant_limit: None,
            pool_max_open: 20,
            tx: false,
            steps,
        });
    }
    if s.batch && p.queries.contains_key("batch_agg") {
        p.actors.push(Actor {
            name: "heavy-batch".into(),
            kind: Kind::Batch,
            every_ms: Some(60_000),
            instances: 1,
            qps_per_tenant: None,
            stagger: false,
            concurrency: Some(1),
            tenant_limit: Some(50),
            pool_max_open: 2,
            tx: false,
            steps: vec![Step {
                id: None,
                query: "batch_agg".into(),
                foreach: None,
                limit: None,
                repeat: None,
                cache_hit: None,
            }],
        });
    }
}
