//! 試算: 実行せずに、クエリの頻度・CPU・接続数・安全上限を出す。
//! 1 クエリの CPU は校正があればそれを使い、無ければ仮置き（EXP-65 の実測 0.42〜0.77ms の幅）で幅を出す。
use crate::model::*;
use crate::schema;
use serde::Serialize;
use std::collections::BTreeMap;

pub const DEFAULT_MS_LOW: f64 = 0.42;
pub const DEFAULT_MS_HIGH: f64 = 0.77;

#[derive(Serialize, Default, Clone, Debug)]
pub struct QueryLoad {
    pub query: String,
    pub qps: f64,
    pub cpu_ms: f64,
    pub calibrated: bool,
    pub cpu_vcpu: f64,
}
#[derive(Serialize, Default, Clone, Debug)]
pub struct ActorLoad {
    pub name: String,
    pub loops_per_s: f64,
    pub queries_per_loop: f64,
    pub cpu_vcpu: f64,
    /// 同時に必要な接続の見積り（リトルの法則）
    pub conns_needed: f64,
    pub pool_max_open: u32,
    pub scales_with_tenants: bool,
}
#[derive(Serialize, Default, Clone, Debug)]
pub struct Estimate {
    pub tenants: u32,
    pub actors: Vec<ActorLoad>,
    pub queries: Vec<QueryLoad>,
    pub total_qps: f64,
    pub cpu_vcpu: f64,
    /// 仮置きの幅（校正が無いクエリがある場合のみ意味がある）
    pub cpu_vcpu_low: f64,
    pub cpu_vcpu_high: f64,
    pub util: f64,
    pub util_low: f64,
    pub util_high: f64,
    pub conns_needed: f64,
    pub conns_pool_max: u32,
    pub conns_budget: i64,
    pub safe_tenants: f64,
    pub safe_tenants_low: f64,
    pub safe_tenants_high: f64,
    pub uncalibrated: Vec<String>,
    pub warnings: Vec<String>,
    /// 単価が設定されているときの月額（730 時間）と、安全上限でのテナントあたりの月額
    pub monthly_cost: Option<f64>,
    pub cost_per_tenant_safe: Option<f64>,
}

fn default_rows(p: &Project, q: &str) -> f64 {
    let t = |n: &str| p.schema.tables.iter().find(|t| t.name == n);
    if q == "tree_join" || q == "tree_in" {
        let g = p
            .schema
            .tables
            .iter()
            .find(|t| schema::depth_of(&p.schema, &t.name) == 3);
        return g
            .map(|g| {
                g.rows as f64
                    * g.parent
                        .as_deref()
                        .and_then(t)
                        .map(|m| m.rows as f64)
                        .unwrap_or(1.0)
            })
            .unwrap_or(1.0);
    }
    if let Some((tn, _)) = q.split_once("_by_").or_else(|| q.split_once("_join_")) {
        return t(tn).map(|t| t.rows as f64).unwrap_or(1.0);
    }
    1.0
}

pub fn estimate(p: &Project, calib: &Calib, tenants: u32) -> Estimate {
    let mut e = Estimate {
        tenants,
        ..Default::default()
    };
    let mut qps: BTreeMap<String, f64> = BTreeMap::new();
    let mut fixed_qps: BTreeMap<String, f64> = BTreeMap::new(); // テナント数に比例しない分（バッチ）
    let rtt = p.profile.measured_rtt_ms.unwrap_or(p.profile.rtt_ms);
    let lat_ms = |q: &str| calib.p50_ms.get(q).copied().unwrap_or(1.0) + rtt;

    for a in &p.actors {
        let n_t = a.tenant_limit.unwrap_or(tenants).min(tenants) as f64;
        let (loops, scales) = match a.kind {
            Kind::Worker => (
                n_t * a.instances.max(1) as f64 * 1000.0 / a.every_ms.unwrap_or(1000).max(1) as f64,
                true,
            ),
            Kind::Web => (tenants as f64 * a.qps_per_tenant.unwrap_or(0.0), true),
            Kind::Sse => match a.every_ms {
                Some(ev) => (n_t * a.connections_per_tenant.unwrap_or(1) as f64 * if a.singleflight { 1.0 / a.connections_per_tenant.unwrap_or(1) as f64 } else { 1.0 } * 1000.0 / ev.max(1) as f64, true),
                None => {
                    let burst = n_t * if a.singleflight { 1.0 } else { a.connections_per_tenant.unwrap_or(1) as f64 };
                    e.warnings.push(format!("SSE '{}' は 1 回きりの一斉接続です（{burst:.0} ループが一度に走る。定常の頻度には含めません。実測で p99 とプール待ちを確認）", a.name));
                    (0.0, false)
                }
            },
            Kind::Batch => match a.every_ms {
                Some(ev) => (
                    a.concurrency.unwrap_or(1) as f64 * 1000.0 / ev.max(1) as f64,
                    false,
                ),
                None => {
                    e.warnings.push(format!(
                        "バッチ '{}' は流しっぱなしのため頻度を試算できません（実測で確認）",
                        a.name
                    ));
                    (0.0, false)
                }
            },
        };
        // 1 ループの各クエリの実行回数
        let mut rows_of: BTreeMap<String, f64> = BTreeMap::new();
        let mut per_q: BTreeMap<String, f64> = BTreeMap::new();
        let mut loop_time_ms = 0.0;
        for s in &a.steps {
            let avg_rows = calib
                .avg_rows
                .get(&s.query)
                .copied()
                .unwrap_or_else(|| default_rows(p, &s.query));
            let mult = match &s.foreach {
                Some(f) => rows_of
                    .get(f)
                    .copied()
                    .unwrap_or(1.0)
                    .min(s.limit.unwrap_or(u32::MAX) as f64),
                None => s.repeat.unwrap_or(1) as f64,
            } * (1.0 - s.cache_hit.unwrap_or(0.0));
            *per_q.entry(s.query.clone()).or_default() += mult;
            loop_time_ms += mult * lat_ms(&s.query);
            if let Some(id) = &s.id {
                rows_of.insert(id.clone(), avg_rows);
            }
        }
        let qpl: f64 = per_q.values().sum();
        for (q, m) in &per_q {
            *(if scales { &mut qps } else { &mut fixed_qps })
                .entry(q.clone())
                .or_default() += m * loops;
        }
        let ms_mid = |q: &str| {
            calib
                .cpu_ms
                .get(q)
                .copied()
                .unwrap_or((DEFAULT_MS_LOW + DEFAULT_MS_HIGH) / 2.0)
        };
        let cpu: f64 = per_q
            .iter()
            .map(|(q, m)| m * loops * ms_mid(q) / 1000.0)
            .sum();
        let conc = if a.tx {
            loops * loop_time_ms / 1000.0
        } else {
            per_q
                .iter()
                .map(|(q, m)| m * loops * lat_ms(q) / 1000.0)
                .sum()
        };
        e.actors.push(ActorLoad {
            name: a.name.clone(),
            loops_per_s: loops,
            queries_per_loop: qpl,
            cpu_vcpu: cpu,
            conns_needed: conc,
            pool_max_open: a.pool_max_open,
            scales_with_tenants: scales,
        });
    }

    let mut all: BTreeMap<String, f64> = qps.clone();
    for (q, v) in &fixed_qps {
        *all.entry(q.clone()).or_default() += v;
    }
    let (mut cpu, mut lo, mut hi, mut fixed_cpu_lo, mut fixed_cpu_hi) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (q, r) in &all {
        let c = calib.cpu_ms.get(q).copied();
        let (m, ml, mh) = match c {
            Some(c) => (c, c, c),
            None => {
                e.uncalibrated.push(q.clone());
                (
                    (DEFAULT_MS_LOW + DEFAULT_MS_HIGH) / 2.0,
                    DEFAULT_MS_LOW,
                    DEFAULT_MS_HIGH,
                )
            }
        };
        cpu += r * m / 1000.0;
        lo += r * ml / 1000.0;
        hi += r * mh / 1000.0;
        if let Some(f) = fixed_qps.get(q) {
            fixed_cpu_lo += f * ml / 1000.0;
            fixed_cpu_hi += f * mh / 1000.0;
        }
        e.queries.push(QueryLoad {
            query: q.clone(),
            qps: *r,
            cpu_ms: m,
            calibrated: c.is_some(),
            cpu_vcpu: r * m / 1000.0,
        });
    }
    e.total_qps = all.values().sum();
    e.cpu_vcpu = cpu;
    e.cpu_vcpu_low = lo;
    e.cpu_vcpu_high = hi;
    let v = p.profile.vcpu.max(0.01);
    e.util = cpu / v;
    e.util_low = lo / v;
    e.util_high = hi / v;
    e.conns_needed = e.actors.iter().map(|a| a.conns_needed).sum();
    e.conns_pool_max = e.actors.iter().map(|a| a.pool_max_open).sum();
    e.conns_budget = p.profile.max_connections as i64 - 20; // 予約（監視・マイグレーション等）
    let budget = p.profile.vcpu * p.profile.safe_util - p.profile.reserve_vcpu;
    let safe = |cpu_total: f64, fixed: f64| {
        let per_tenant = (cpu_total - fixed) / tenants.max(1) as f64;
        if per_tenant <= 0.0 {
            f64::INFINITY
        } else {
            ((budget - fixed) / per_tenant).max(0.0)
        }
    };
    e.safe_tenants = safe(cpu, (fixed_cpu_lo + fixed_cpu_hi) / 2.0);
    e.safe_tenants_low = safe(hi, fixed_cpu_hi); // CPU が重い側 → 上限は小さい
    e.safe_tenants_high = safe(lo, fixed_cpu_lo);
    if let Some(pr) = p.profile.price_per_vcpu_hour {
        let mc = p.profile.vcpu * pr * 730.0;
        e.monthly_cost = Some(mc);
        if e.safe_tenants.is_finite() && e.safe_tenants > 0.0 {
            e.cost_per_tenant_safe = Some(mc / e.safe_tenants);
        }
    }
    if !e.uncalibrated.is_empty() {
        e.warnings.push(format!("{} 個のクエリが未校正です。仮置き（{DEFAULT_MS_LOW}〜{DEFAULT_MS_HIGH} ms/回、EXP-65 の実測の幅）で幅を出しています。「校正」を実行すると実測に置き換わります", e.uncalibrated.len()));
    }
    if e.conns_pool_max as i64 > e.conns_budget {
        e.warnings.push(format!("プール上限の合計 {} が接続の予算 {} を超えています（実際に張られるのは使った分だけ。同時に必要な見積りは {:.0}）", e.conns_pool_max, e.conns_budget, e.conns_needed));
    }
    for a in &e.actors {
        if a.conns_needed > a.pool_max_open as f64 {
            e.warnings.push(format!("'{}' は同時に約 {:.0} 本の接続が要る見積りですが、プール上限が {} です（待ちが出ます）", a.name, a.conns_needed, a.pool_max_open));
        }
    }
    if e.util > 0.88 {
        e.warnings
            .push("使用率が 88% を超えています。待ち行列が急に伸びる領域です（EXP-65）".into());
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        let mut p = Project {
            name: "t".into(),
            tenants: 1000,
            ..Default::default()
        };
        p.queries.insert(
            "poll".into(),
            QueryDef {
                sql: "select 1".into(),
                write: false,
            },
        );
        p.actors.push(Actor {
            name: "w1".into(),
            kind: Kind::Worker,
            every_ms: Some(1000),
            instances: 1,
            qps_per_tenant: None,
            stagger: true,
            concurrency: None,
            tenant_limit: None,
            pool_max_open: 10,
            target: Target::Writer,
 tenant_skew: None,
 shape: None,
 hot_rows: None,
 connections_per_tenant: None,
 singleflight: false,
            tx: false,
            steps: vec![Step {
                id: None,
                query: "poll".into(),
                foreach: None,
                limit: None,
                repeat: None,
                cache_hit: None,
            }],
        });
        p
    }

    /// EXP-65: 1 秒ポーリング 2,000 テナントは、0.42〜0.77ms/回で CPU 使用率 42〜77%（実測 75〜80%）
    #[test]
    fn exp65_2000_tenants_polling() {
        let mut p = project();
        p.tenants = 2000;
        let e = estimate(&p, &Calib::default(), 2000);
        assert!((e.total_qps - 2000.0).abs() < 1e-6);
        assert!(
            (e.util_low - 0.42).abs() < 0.01 && (e.util_high - 0.77).abs() < 0.01,
            "{e:?}"
        );
        assert_eq!(e.uncalibrated, vec!["poll".to_string()]);
    }

    /// 校正があれば幅は消え、安全上限は (2×0.6−0.1)/(0.6ms/1000) = 1833
    #[test]
    fn calibrated_safe_limit() {
        let p = project();
        let mut c = Calib::default();
        c.cpu_ms.insert("poll".into(), 0.6);
        let e = estimate(&p, &c, 1000);
        assert!((e.safe_tenants - 1833.3).abs() < 1.0, "{}", e.safe_tenants);
        assert!((e.safe_tenants_low - e.safe_tenants_high).abs() < 1e-6);
    }

    /// 親→子→孫: 親 1 + 子 1 + 孫は子の行数ぶん（上限つき）= 1 ループ 12 クエリ
    #[test]
    fn nplus1_counts() {
        let mut p = project();
        for (n, par, rows) in [
            ("sites", None, 100u32),
            ("devices", Some("sites"), 10),
            ("readings", Some("devices"), 20),
        ] {
            p.schema.tables.push(TableSpec {
                name: n.into(),
                parent: par.map(|s| s.to_string()),
                columns: 3,
                size: Size::S,
                rows,
            });
        }
        p.queries = schema::candidates(&p);
        p.actors[0].steps = vec![
            Step {
                id: Some("sites".into()),
                query: "sites_pk".into(),
                foreach: None,
                limit: None,
                repeat: None,
                cache_hit: None,
            },
            Step {
                id: Some("devices".into()),
                query: "devices_by_sites".into(),
                foreach: None,
                limit: None,
                repeat: None,
                cache_hit: None,
            },
            Step {
                id: None,
                query: "readings_by_devices".into(),
                foreach: Some("devices".into()),
                limit: Some(20),
                repeat: None,
                cache_hit: None,
            },
        ];
        let e = estimate(&p, &Calib::default(), 100);
        assert!(
            (e.actors[0].queries_per_loop - 12.0).abs() < 1e-9,
            "{:?}",
            e.actors[0]
        );
    }

    /// 認証のキャッシュ率が効く
    #[test]
    fn auth_cache_hit() {
        let mut p = project();
        p.queries.insert(
            "auth".into(),
            QueryDef {
                sql: "select 1".into(),
                write: false,
            },
        );
        p.actors.clear();
        p.actors.push(Actor {
            name: "web".into(),
            kind: Kind::Web,
            every_ms: None,
            instances: 1,
            qps_per_tenant: Some(1.0),
            stagger: false,
            concurrency: None,
            tenant_limit: None,
            pool_max_open: 10,
            target: Target::Writer,
 tenant_skew: None,
 shape: None,
 hot_rows: None,
 connections_per_tenant: None,
 singleflight: false,
            tx: false,
            steps: vec![Step {
                id: None,
                query: "auth".into(),
                foreach: None,
                limit: None,
                repeat: None,
                cache_hit: Some(0.98),
            }],
        });
        let e = estimate(&p, &Calib::default(), 1000);
        assert!((e.total_qps - 20.0).abs() < 1e-6, "{}", e.total_qps);
    }

    /// 接続: トランザクション内の外部待ちが伸びると、必要な接続数がプール上限を超える（EXP-66 のプール枯渇）
    #[test]
    fn pool_exhaustion_warning() {
        let mut p = project();
        p.profile.rtt_ms = 50.0;
        p.actors[0].tx = true;
        p.actors[0].every_ms = Some(2000); // 500 loops/s
        p.actors[0].pool_max_open = 10;
        let e = estimate(&p, &Calib::default(), 1000);
        assert!(e.actors[0].conns_needed > 10.0, "{:?}", e.actors[0]);
        assert!(e.warnings.iter().any(|w| w.contains("プール上限")));
    }
}
