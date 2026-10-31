//! エンジン差分（MySQL / PostgreSQL）を閉じ込める。
use crate::model::{EngineKind, TlsMode};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;

#[derive(Default, Debug, Clone)]
pub struct RunRes {
    pub rows: u64,
    /// 1 列目を整数として読めた値（親→子→孫のループに使う）
    pub ids: Vec<i64>,
}

#[async_trait]
pub trait Conn: Send {
    async fn exec(&mut self, sql: &str) -> Result<RunRes>;
    /// プリペアドで実行する。`cache` が true なら接続ごとにステートメントを再利用、false なら毎回 prepare → execute → close
    async fn exec_params(&mut self, sql: &str, params: &[i32], cache: bool) -> Result<RunRes>;
    /// 結果を文字列の表で返す（状態採取など）
    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<String>>>;
    /// 列名つきの表（Proxy の管理コンソールなど、列の並びがバージョンで変わるもの用）
    async fn rows_named(&mut self, sql: &str) -> Result<Vec<std::collections::HashMap<String, String>>>;
}

#[derive(Clone)]
pub struct Engine {
    pub kind: EngineKind,
    /// アプリの接続が使う接続先（遅延プロキシ経由のこともある）
    pub dsn: String,
    /// 状態採取・障害注入・設定取得が使う、プロキシを通さない接続先
    pub direct: String,
    pub tls: TlsMode,
}

impl Engine {
    pub fn new(kind: EngineKind, dsn: String) -> Engine {
        Engine { kind, direct: dsn.clone(), dsn, tls: TlsMode::Off }
    }

    pub fn with_tls(mut self, tls: TlsMode) -> Engine {
        self.tls = tls;
        self
    }

    pub fn with_dsn(&self, dsn: String) -> Engine {
        Engine { kind: self.kind, dsn, direct: self.direct.clone(), tls: self.tls }
    }

    pub async fn connect_direct(&self) -> Result<Box<dyn Conn>> {
        self.with_dsn(self.direct.clone()).connect().await
    }

    /// search_path などの設定をせずに接続する（Proxy の管理コンソール用）
    pub async fn connect_raw(&self) -> Result<Box<dyn Conn>> {
        match self.kind {
            EngineKind::Postgres => Ok(Box::new(Pg::connect(&self.dsn, self.tls).await?)),
            EngineKind::Mysql => Ok(Box::new(My::connect(&self.dsn, self.tls).await?)),
        }
    }

    pub async fn connect(&self) -> Result<Box<dyn Conn>> {
        match self.kind {
            EngineKind::Postgres => {
                let mut c = Pg::connect(&self.dsn, self.tls).await?;
                // 既存の表に触れないよう、専用スキーマ caplab だけを使う
                c.exec("CREATE SCHEMA IF NOT EXISTS caplab").await?;
                c.exec("SET search_path TO caplab").await?;
                Ok(Box::new(c))
            }
            EngineKind::Mysql => Ok(Box::new(My::connect(&self.dsn, self.tls).await?)),
        }
    }

    /// DB 側の状態（接続数・実行中・読み取りの統計）を数値で返す
    pub async fn snapshot(&self, c: &mut dyn Conn) -> Result<BTreeMap<String, f64>> {
        let mut m = BTreeMap::new();
        match self.kind {
            EngineKind::Postgres => {
                let r = c.rows("select count(*) filter (where backend_type='client backend' and datname=current_database()), \
                    count(*) filter (where state='active' and backend_type='client backend' and datname=current_database() and pid<>pg_backend_pid()) \
                    from pg_stat_activity").await?;
                if let Some(row) = r.first() {
                    m.insert("db.conns".into(), row[0].parse().unwrap_or(0.0));
                    m.insert("db.active".into(), row[1].parse().unwrap_or(0.0));
                }
                let r = c.rows("select blks_hit, blks_read, xact_commit, tup_returned, tup_fetched from pg_stat_database where datname=current_database()").await?;
                if let Some(row) = r.first() {
                    m.insert("db.blks_hit".into(), row[0].parse().unwrap_or(0.0));
                    m.insert("db.blks_read".into(), row[1].parse().unwrap_or(0.0));
                    m.insert("db.xact".into(), row[2].parse().unwrap_or(0.0));
                    m.insert("db.rows_read".into(), row[3].parse().unwrap_or(0.0));
                }
            }
            EngineKind::Mysql => {
                let r = c.rows("SHOW GLOBAL STATUS WHERE Variable_name IN ('Threads_connected','Threads_running','Innodb_buffer_pool_reads','Innodb_buffer_pool_read_requests','Innodb_row_lock_waits','Innodb_rows_read','Questions')").await?;
                for row in r {
                    let v: f64 = row[1].parse().unwrap_or(0.0);
                    match row[0].as_str() {
                        "Threads_connected" => m.insert("db.conns".into(), v),
                        "Threads_running" => m.insert("db.active".into(), (v - 1.0).max(0.0)), // 自分を除く
                        "Innodb_buffer_pool_reads" => m.insert("db.blks_read".into(), v),
                        "Innodb_buffer_pool_read_requests" => m.insert("db.blks_hit".into(), v),
                        "Innodb_row_lock_waits" => m.insert("db.lock_waits".into(), v),
                        "Innodb_rows_read" => m.insert("db.rows_read".into(), v),
                        "Questions" => m.insert("db.xact".into(), v),
                        _ => None,
                    };
                }
            }
        }
        Ok(m)
    }

    /// (データ + インデックスの大きさ MB, DB のメモリキャッシュ MB)。データがキャッシュに収まるかの判断に使う
    pub async fn storage_mb(&self, c: &mut dyn Conn) -> Result<(f64, f64)> {
        let (a, b) = match self.kind {
            EngineKind::Postgres => (
                "select coalesce(sum(pg_total_relation_size(c.oid)),0)::float8 / 1048576 from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname='caplab' and c.relkind='r'",
                "select pg_size_bytes(current_setting('shared_buffers'))::float8 / 1048576",
            ),
            EngineKind::Mysql => (
                "select coalesce(sum(data_length+index_length),0) / 1048576 from information_schema.tables where table_schema=database()",
                "select @@innodb_buffer_pool_size / 1048576",
            ),
        };
        let f = |r: Vec<Vec<String>>| r.first().and_then(|x| x[0].parse::<f64>().ok()).unwrap_or(0.0);
        Ok((f(c.rows(a).await?), f(c.rows(b).await?)))
    }

    /// EXPLAIN の結果を (全件走査の表の数, 要約) にする
    pub async fn explain(&self, c: &mut dyn Conn, sql: &str) -> Result<(u32, String)> {
        match self.kind {
            EngineKind::Postgres => {
                let r = c.rows(&format!("EXPLAIN (FORMAT JSON) {sql}")).await?;
                let txt = r.first().map(|x| x[0].clone()).unwrap_or_default();
                let n = txt.matches("\"Seq Scan\"").count() as u32;
                let idx = txt.matches("Index").count() as u32;
                Ok((n, format!("{} 個の全件走査 / インデックス利用 {} 箇所", n, idx)))
            }
            EngineKind::Mysql => {
                let r = c.rows(&format!("EXPLAIN FORMAT=JSON {sql}")).await?;
                let txt = r.first().map(|x| x[0].clone()).unwrap_or_default();
                let n = txt.matches("\"access_type\": \"ALL\"").count() as u32;
                let idx = txt.matches("\"key\"").count() as u32;
                Ok((n, format!("{} 個の全件走査 / インデックス利用 {} 箇所", n, idx)))
            }
        }
    }

    /// この接続が実際に暗号化されているか
    pub async fn tls_in_use(&self, c: &mut dyn Conn) -> Result<bool> {
        Ok(match self.kind {
            EngineKind::Postgres => c.rows("select ssl::text from pg_stat_ssl where pid=pg_backend_pid()").await?.first().map(|r| r[0] == "true" || r[0] == "t").unwrap_or(false),
            EngineKind::Mysql => c.rows("SHOW SESSION STATUS LIKE 'Ssl_cipher'").await?.first().map(|r| !r[1].is_empty()).unwrap_or(false),
        })
    }

    /// このアプリの接続をすべて強制切断する（自分の接続は除く）。切断した数を返す
    pub async fn kill_connections(&self) -> Result<u64> {
        let mut c = self.connect_direct().await?;
        match self.kind {
            EngineKind::Postgres => {
                let r = c.rows("select count(pg_terminate_backend(pid)) from pg_stat_activity where datname=current_database() and pid<>pg_backend_pid() and backend_type='client backend'").await?;
                Ok(r.first().and_then(|x| x[0].parse().ok()).unwrap_or(0))
            }
            EngineKind::Mysql => {
                let ids = c.rows("SELECT id FROM information_schema.processlist WHERE id<>CONNECTION_ID() AND db=DATABASE()").await?;
                let mut n = 0;
                for row in ids {
                    if c.exec(&format!("KILL {}", row[0])).await.is_ok() {
                        n += 1;
                    }
                }
                Ok(n)
            }
        }
    }

    /// 測定条件として残す設定値（耐久性の設定を含む）
    pub async fn settings(&self, c: &mut dyn Conn) -> Result<BTreeMap<String, String>> {
        let mut m = BTreeMap::new();
        let sql = match self.kind {
            EngineKind::Postgres => "select name, setting from pg_settings where name in ('fsync','synchronous_commit','shared_buffers','max_connections','server_version')",
            EngineKind::Mysql => "SHOW VARIABLES WHERE Variable_name IN ('innodb_flush_log_at_trx_commit','sync_binlog','innodb_buffer_pool_size','max_connections','version')",
        };
        for row in c.rows(sql).await? {
            m.insert(row[0].clone(), row[1].clone());
        }
        Ok(m)
    }
}

// ---------------- PostgreSQL ----------------
pub struct Pg {
    client: tokio_postgres::Client,
    cache: std::collections::HashMap<String, tokio_postgres::Statement>,
}
/// 証明書を検証しない（暗号化だけ行う）検証器
#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _e: &rustls::pki_types::CertificateDer<'_>, _i: &[rustls::pki_types::CertificateDer<'_>], _s: &rustls::pki_types::ServerName<'_>, _o: &[u8], _n: rustls::pki_types::UnixTime) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _m: &[u8], _c: &rustls::pki_types::CertificateDer<'_>, _d: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _m: &[u8], _c: &rustls::pki_types::CertificateDer<'_>, _d: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

fn rustls_config(tls: TlsMode) -> Result<rustls::ClientConfig> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let b = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|e| anyhow!("TLS 設定: {e}"))?;
    if tls == TlsMode::Insecure {
        return Ok(b.dangerous().with_custom_certificate_verifier(std::sync::Arc::new(NoVerify)).with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Ok(path) = std::env::var("CAPLAB_TLS_CA") {
        let f = std::fs::File::open(&path).map_err(|e| anyhow!("CAPLAB_TLS_CA '{path}' を開けません: {e}"))?;
        for c in rustls_pemfile::certs(&mut std::io::BufReader::new(f)).flatten() {
            let _ = roots.add(c);
        }
    }
    Ok(b.with_root_certificates(roots).with_no_client_auth())
}

impl Pg {
    async fn connect(dsn: &str, tls: TlsMode) -> Result<Pg> {
        let mut cfg: tokio_postgres::Config = dsn.parse().map_err(|e| anyhow!("postgres の接続文字列が不正: {e}"))?;
        let client = match tls {
            TlsMode::Off => {
                cfg.ssl_mode(tokio_postgres::config::SslMode::Disable);
                let (client, conn) = cfg.connect(tokio_postgres::NoTls).await.map_err(|e| anyhow!("postgres 接続失敗: {e}"))?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                client
            }
            _ => {
                cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
                let connector = tokio_postgres_rustls::MakeRustlsConnect::new(rustls_config(tls)?);
                let (client, conn) = cfg.connect(connector).await.map_err(|e| anyhow!("postgres TLS 接続失敗: {e}"))?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                client
            }
        };
        Ok(Pg { client, cache: Default::default() })
    }
}

fn is_write_sql(sql: &str) -> bool {
    let s = sql.trim_start().to_ascii_lowercase();
    s.starts_with("update") || s.starts_with("insert") || s.starts_with("delete")
}

#[async_trait]
impl Conn for Pg {
    async fn exec_params(&mut self, sql: &str, params: &[i32], cache: bool) -> Result<RunRes> {
        use tokio_postgres::types::{ToSql, Type};
        let stmt = if cache {
            match self.cache.get(sql) {
                Some(s) => s.clone(),
                None => {
                    let s = self.client.prepare(sql).await?;
                    self.cache.insert(sql.to_string(), s.clone());
                    s
                }
            }
        } else {
            self.client.prepare(sql).await?
        };
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        if is_write_sql(sql) {
            let n = self.client.execute(&stmt, &refs).await?;
            return Ok(RunRes { rows: n, ids: vec![] });
        }
        let rows = self.client.query(&stmt, &refs).await?;
        let mut r = RunRes { rows: rows.len() as u64, ids: vec![] };
        for row in &rows {
            let v = match *row.columns()[0].type_() {
                Type::INT4 => row.try_get::<_, Option<i32>>(0).ok().flatten().map(|x| x as i64),
                Type::INT8 => row.try_get::<_, Option<i64>>(0).ok().flatten(),
                _ => None,
            };
            if let Some(v) = v {
                r.ids.push(v);
            }
        }
        Ok(r)
    }
    async fn exec(&mut self, sql: &str) -> Result<RunRes> {
        use tokio_postgres::SimpleQueryMessage as M;
        let msgs = self.client.simple_query(sql).await?;
        let mut r = RunRes::default();
        for m in msgs {
            match m {
                M::Row(row) => {
                    r.rows += 1;
                    if let Some(Some(v)) = row.try_get(0).ok().map(|x| x) {
                        if let Ok(i) = v.parse::<i64>() {
                            r.ids.push(i);
                        }
                    }
                }
                M::CommandComplete(n) => {
                    if r.rows == 0 {
                        r.rows = n;
                    }
                }
                _ => {}
            }
        }
        Ok(r)
    }
    async fn rows_named(&mut self, sql: &str) -> Result<Vec<std::collections::HashMap<String, String>>> {
        use tokio_postgres::SimpleQueryMessage as M;
        let mut out = vec![];
        for m in self.client.simple_query(sql).await? {
            if let M::Row(row) = m {
                out.push(row.columns().iter().enumerate().map(|(i, c)| (c.name().to_string(), row.get(i).unwrap_or("").to_string())).collect());
            }
        }
        Ok(out)
    }
    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<String>>> {
        use tokio_postgres::SimpleQueryMessage as M;
        let mut out = vec![];
        for m in self.client.simple_query(sql).await? {
            if let M::Row(row) = m {
                out.push(
                    (0..row.len())
                        .map(|i| row.get(i).unwrap_or("").to_string())
                        .collect(),
                );
            }
        }
        Ok(out)
    }
}

// ---------------- MySQL ----------------
pub struct My {
    conn: mysql_async::Conn,
}
impl My {
    async fn connect(dsn: &str, tls: TlsMode) -> Result<My> {
        let opts = mysql_async::Opts::from_url(dsn)
            .map_err(|e| anyhow!("mysql の接続文字列が不正: {e}"))?;
        // ローカル接続でも TCP のまま使う（既定は接続後に Unix ソケットへ切り替わり、遅延プロキシや本番と違う経路になる）
        let opts: mysql_async::Opts = mysql_async::OptsBuilder::from_opts(opts)
            .prefer_socket(std::env::var("CAPLAB_MYSQL_SOCKET").is_ok())
            .tcp_nodelay(true)
            .ssl_opts(match tls {
                TlsMode::Off => None,
                TlsMode::Insecure => Some(mysql_async::SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
                TlsMode::Verify => {
                    let o = mysql_async::SslOpts::default();
                    Some(match std::env::var("CAPLAB_TLS_CA") {
                        Ok(p) => o.with_root_certs(vec![std::path::PathBuf::from(p).into()]),
                        Err(_) => o,
                    })
                }
            })
            .into();
        let conn = mysql_async::Conn::new(opts)
            .await
            .map_err(|e| anyhow!("mysql 接続失敗: {e}"))?;
        Ok(My { conn })
    }
}
fn my_res(rows: Vec<mysql_async::Row>, affected: u64) -> RunRes {
    let mut r = RunRes { rows: if rows.is_empty() { affected } else { rows.len() as u64 }, ids: vec![] };
    for row in &rows {
        if let Some(v) = row.as_ref(0) {
            if let Ok(i) = mysql_async::from_value_opt::<i64>(v.clone()) {
                r.ids.push(i);
            }
        }
    }
    r
}

#[async_trait]
impl Conn for My {
    async fn exec_params(&mut self, sql: &str, params: &[i32], cache: bool) -> Result<RunRes> {
        use mysql_async::prelude::Queryable;
        let vals: Vec<mysql_async::Value> = params.iter().map(|p| (*p).into()).collect();
        if cache {
            let mut res = self.conn.exec_iter(sql, vals).await?;
            let affected = res.affected_rows();
            let rows: Vec<mysql_async::Row> = res.collect_and_drop().await?;
            Ok(my_res(rows, affected))
        } else {
            let st = self.conn.prep(sql).await?;
            let out = {
                let mut res = self.conn.exec_iter(&st, vals).await?;
                let affected = res.affected_rows();
                let rows: Vec<mysql_async::Row> = res.collect_and_drop().await?;
                my_res(rows, affected)
            };
            self.conn.close(st).await?;
            Ok(out)
        }
    }
    async fn exec(&mut self, sql: &str) -> Result<RunRes> {
        use mysql_async::prelude::Queryable;
        let mut res = self.conn.query_iter(sql).await?;
        let affected = res.affected_rows();
        let rows: Vec<mysql_async::Row> = res.collect_and_drop().await?;
        let mut r = RunRes {
            rows: if rows.is_empty() {
                affected
            } else {
                rows.len() as u64
            },
            ids: vec![],
        };
        for row in &rows {
            if let Some(v) = row.as_ref(0) {
                if let Ok(i) = mysql_async::from_value_opt::<i64>(v.clone()) {
                    r.ids.push(i);
                }
            }
        }
        Ok(r)
    }
    async fn rows_named(&mut self, sql: &str) -> Result<Vec<std::collections::HashMap<String, String>>> {
        use mysql_async::prelude::Queryable;
        let rows: Vec<mysql_async::Row> = self.conn.query(sql).await?;
        Ok(rows
            .iter()
            .map(|row| {
                row.columns_ref()
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.name_str().to_string(), mysql_async::from_value_opt::<String>(row.as_ref(i).cloned().unwrap_or(mysql_async::Value::NULL)).unwrap_or_default()))
                    .collect()
            })
            .collect())
    }
    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<String>>> {
        use mysql_async::prelude::Queryable;
        let rows: Vec<mysql_async::Row> = self.conn.query(sql).await?;
        Ok(rows
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| {
                        mysql_async::from_value_opt::<String>(
                            row.as_ref(i).cloned().unwrap_or(mysql_async::Value::NULL),
                        )
                        .unwrap_or_default()
                    })
                    .collect()
            })
            .collect())
    }
}
