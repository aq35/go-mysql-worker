//! Proxy（PgBouncer / ProxySQL）の状態採取。アプリ → Proxy → DB の接続数を 3 層で見るための 2 層目。
use crate::model::{ProxyCfg, ProxyKind};
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;

pub struct ProxyObserver {
    cfg: ProxyCfg,
    conn: Option<Box<dyn crate::engine::Conn>>,
}

impl ProxyObserver {
    pub fn new(cfg: ProxyCfg) -> ProxyObserver {
        ProxyObserver { cfg, conn: None }
    }

    async fn connect(&mut self) -> Result<()> {
        let kind = match self.cfg.kind {
            ProxyKind::Pgbouncer => crate::model::EngineKind::Postgres,
            ProxyKind::Proxysql => crate::model::EngineKind::Mysql,
        };
        let e = crate::engine::Engine::new(kind, self.cfg.admin_dsn.clone());
        self.conn = Some(e.connect_raw().await?);
        Ok(())
    }

    /// proxy.clients（アプリ → Proxy）, proxy.sv_active / proxy.sv_idle（Proxy → DB）, proxy.cl_waiting（待たされているクライアント）
    pub async fn sample(&mut self) -> Result<BTreeMap<String, f64>> {
        if self.conn.is_none() {
            self.connect().await?;
        }
        let r = self.sample_inner().await;
        if r.is_err() {
            self.conn = None;
        }
        r
    }

    async fn sample_inner(&mut self) -> Result<BTreeMap<String, f64>> {
        let c = self.conn.as_mut().unwrap();
        let mut m = BTreeMap::new();
        match self.cfg.kind {
            ProxyKind::Pgbouncer => {
                // 列名つきで読む（バージョンで列の並びが変わるため）
                let rows = c.rows_named("SHOW POOLS").await?;
                let (mut sa, mut si, mut cw, mut ca) = (0.0, 0.0, 0.0, 0.0);
                for row in rows.iter().filter(|r| r.get("database").map(|d| d != "pgbouncer").unwrap_or(false)) {
                    let g = |k: &str| row.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                    sa += g("sv_active");
                    si += g("sv_idle") + g("sv_used");
                    cw += g("cl_waiting");
                    ca += g("cl_active");
                }
                let lists = c.rows("SHOW LISTS").await?;
                let used = lists.iter().find(|r| r[0] == "used_clients").and_then(|r| r[1].parse::<f64>().ok()).unwrap_or(0.0);
                m.insert("proxy.clients".into(), (used - 1.0).max(0.0)); // 自分（管理接続）を除く
                m.insert("proxy.cl_active".into(), ca);
                m.insert("proxy.cl_waiting".into(), cw);
                m.insert("proxy.sv_active".into(), sa);
                m.insert("proxy.sv_idle".into(), si);
            }
            ProxyKind::Proxysql => {
                let pool = c.rows("SELECT COALESCE(SUM(ConnUsed),0), COALESCE(SUM(ConnFree),0) FROM stats_mysql_connection_pool").await?;
                if let Some(r) = pool.first() {
                    m.insert("proxy.sv_active".into(), r[0].parse().unwrap_or(0.0));
                    m.insert("proxy.sv_idle".into(), r[1].parse().unwrap_or(0.0));
                }
                let g = c.rows("SELECT Variable_Name, Variable_Value FROM stats_mysql_global WHERE Variable_Name IN ('Client_Connections_connected')").await?;
                for r in g {
                    if r[0] == "Client_Connections_connected" {
                        m.insert("proxy.clients".into(), r[1].parse().unwrap_or(0.0));
                    }
                }
            }
        }
        if m.is_empty() {
            return Err(anyhow!("proxy の状態を取得できませんでした"));
        }
        Ok(m)
    }
}
