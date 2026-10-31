//! エンジン差分（MySQL / PostgreSQL）を閉じ込める。
use crate::model::EngineKind;
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
    /// 結果を文字列の表で返す（状態採取など）
    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<String>>>;
}

#[derive(Clone)]
pub struct Engine {
    pub kind: EngineKind,
    pub dsn: String,
}

impl Engine {
    pub fn new(kind: EngineKind, dsn: String) -> Engine {
        Engine { kind, dsn }
    }

    pub async fn connect(&self) -> Result<Box<dyn Conn>> {
        match self.kind {
            EngineKind::Postgres => {
                let mut c = Pg::connect(&self.dsn).await?;
                // 既存の表に触れないよう、専用スキーマ caplab だけを使う
                c.exec("CREATE SCHEMA IF NOT EXISTS caplab").await?;
                c.exec("SET search_path TO caplab").await?;
                Ok(Box::new(c))
            }
            EngineKind::Mysql => Ok(Box::new(My::connect(&self.dsn).await?)),
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
}
impl Pg {
    async fn connect(dsn: &str) -> Result<Pg> {
        let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
            .await
            .map_err(|e| anyhow!("postgres 接続失敗: {e}"))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(Pg { client })
    }
}
#[async_trait]
impl Conn for Pg {
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
    async fn connect(dsn: &str) -> Result<My> {
        let opts = mysql_async::Opts::from_url(dsn)
            .map_err(|e| anyhow!("mysql の接続文字列が不正: {e}"))?;
        let conn = mysql_async::Conn::new(opts)
            .await
            .map_err(|e| anyhow!("mysql 接続失敗: {e}"))?;
        Ok(My { conn })
    }
}
#[async_trait]
impl Conn for My {
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
