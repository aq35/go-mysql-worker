//! プロジェクト定義（1 プロジェクト = 1 つの JSON）。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 接続の暗号化。RDS / Aurora は TLS が前提のことが多い
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// 暗号化しない（ローカルの実験向け）
    #[default]
    Off,
    /// 暗号化するが、サーバの証明書は検証しない（動作確認用）
    Insecure,
    /// 暗号化して、サーバの証明書を検証する（CA は環境変数 CAPLAB_TLS_CA の PEM。無ければ一般的なルート CA）
    Verify,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Postgres,
    Mysql,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub vcpu: f64,
    pub max_connections: u32,
    /// 安全に使う CPU 使用率（既定 0.6）
    pub safe_util: f64,
    /// テナント処理以外（監視・マイグレーション・autovacuum 等）に取っておく vCPU
    pub reserve_vcpu: f64,
    /// 往復遅延 RTT（ms）。net_mode に従って演出する。0 なら演出しない
    pub rtt_ms: f64,
    /// 遅延の演出方式: sleep = 各クエリの後に待つ簡易版 / proxy = 中間サーバ（接続確立にも遅延、精度は probe で確認）
    #[serde(default)]
    pub net_mode: NetMode,
    /// 遅延のゆらぎ（片方向の最大 ms、一様分布）
    #[serde(default)]
    pub jitter_ms: f64,
    /// vCPU あたりの時間単価（任意。入れると月額と、安全上限でのテナントあたりの費用を出す。通貨は問わない）
    #[serde(default)]
    pub price_per_vcpu_hour: Option<f64>,
    /// probe で実測した RTT（あれば試算に使う）
    #[serde(default)]
    pub measured_rtt_ms: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    #[default]
    Sleep,
    Proxy,
}
impl Default for Profile {
    fn default() -> Self {
        Profile {
            vcpu: 2.0,
            max_connections: 200,
            safe_util: 0.6,
            reserve_vcpu: 0.1,
            rtt_ms: 0.0,
            net_mode: NetMode::Sleep,
            jitter_ms: 0.0,
            price_per_vcpu_hour: None,
            measured_rtt_ms: None,
        }
    }
}

/// クエリの送り方（本物のアプリに合わせる）。Proxy のピン留めや DB の CPU に効く
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// 値を SQL に埋め込んで送る（簡易プロトコル）
    #[default]
    Text,
    /// サーバ側プリペア + 接続ごとにキャッシュして再利用
    Prepared,
    /// 毎回 prepare → execute → close（Go の go-sql-driver/mysql が引数つきクエリで既定でやる形）
    PreparedEach,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// 小 ≈ 100 B / 行
    S,
    /// 中 ≈ 1 KB / 行
    M,
    /// 大 ≈ 10 KB / 行
    L,
}
impl Size {
    pub fn row_bytes(self) -> usize {
        match self {
            Size::S => 100,
            Size::M => 1_000,
            Size::L => 10_000,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TableSpec {
    pub name: String,
    /// 親テーブル名（無ければルート）
    pub parent: Option<String>,
    /// 主キー・親キー・tenant_id を除いたカラム数
    pub columns: u32,
    pub size: Size,
    /// ルート: 1 テナントあたりの件数 / 子・孫: 親 1 件あたりの件数
    pub rows: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Schema {
    pub tables: Vec<TableSpec>,
    /// 外部キーにインデックスを付ける（外すと遅くなる実験ができる）
    #[serde(default = "yes")]
    pub fk_index: bool,
    /// 認証用のセッション表を自動で作る（1 テナントあたりのユーザー数）。None なら作らない
    #[serde(default)]
    pub auth_users_per_tenant: Option<u32>,
}
fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QueryDef {
    /// `:tenant` `:rand_<表>`（その表の乱数 id）`:<ステップid>`（そのステップの結果の id）を使える
    pub sql: String,
    #[serde(default)]
    pub write: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Step {
    #[serde(default)]
    pub id: Option<String>,
    pub query: String,
    /// 指定したステップの結果の各行について実行する（N+1）
    #[serde(default)]
    pub foreach: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub repeat: Option<u32>,
    /// キャッシュで DB を引かない確率（認証など）
    #[serde(default)]
    pub cache_hit: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Worker,
    Web,
    Batch,
    /// SSE / WebSocket の一斉接続（初期スナップショットの取得）。配信そのものは DB を引かない前提
    Sse,
}

/// Web の負荷の時間変化（計測開始からの秒）。倍率は qps_per_tenant に掛かる
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Shape {
    Constant,
    /// 計測期間を通して from 倍から to 倍へ直線的に変える（限界を探る）
    Ramp { from: f64, to: f64 },
    /// at_s 秒から dur_s 秒の間だけ factor 倍（突発的なアクセス）
    Spike { at_s: f64, factor: f64, dur_s: f64 },
}
impl Shape {
    pub fn mult(&self, t: f64, total: f64) -> f64 {
        match self {
            Shape::Constant => 1.0,
            Shape::Ramp { from, to } => from + (to - from) * (t / total.max(1.0)).clamp(0.0, 1.0),
            Shape::Spike { at_s, factor, dur_s } => {
                if t >= *at_s && t < at_s + dur_s { *factor } else { 1.0 }
            }
        }
    }
    pub fn peak(&self) -> f64 {
        match self {
            Shape::Constant => 1.0,
            Shape::Ramp { from, to } => from.max(*to),
            Shape::Spike { factor, .. } => factor.max(1.0),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Actor {
    pub name: String,
    pub kind: Kind,
    /// worker: 実行間隔（ms）。テナントごと・インスタンスごとに 1 本
    #[serde(default)]
    pub every_ms: Option<u64>,
    #[serde(default = "one")]
    pub instances: u32,
    /// web: 1 テナントあたりのリクエスト/秒（全体はポアソン到着）
    #[serde(default)]
    pub qps_per_tenant: Option<f64>,
    /// worker の起動時刻をテナントごとにずらす
    #[serde(default = "yes")]
    pub stagger: bool,
    /// batch: 同時に流す本数（流しっぱなし）、every_ms があれば定期
    #[serde(default)]
    pub concurrency: Option<u32>,
    /// batch / worker が対象にするテナント数の上限（None なら全テナント。batch は 1..=N の範囲を対象にしたいとき）
    #[serde(default)]
    pub tenant_limit: Option<u32>,
    pub pool_max_open: u32,
    /// 接続先（Aurora の Writer / Reader の分離）。Reader は reader_dsn に向く
    #[serde(default)]
    pub target: Target,
    /// テナントの選ばれ方の偏り（Zipf の指数。0 または未指定で一様。1.0 前後で一部のテナントに集中）
    #[serde(default)]
    pub tenant_skew: Option<f64>,
    /// 負荷の時間変化（web のみ）
    #[serde(default)]
    pub shape: Option<Shape>,
    /// `:rand_表` の id を先頭の K 件に絞る（特定の行への集中＝ホットロウ。書き込みの競合を見る）
    #[serde(default)]
    pub hot_rows: Option<u32>,
    /// sse: 1 テナントあたりの接続数（一斉に初期取得する）
    #[serde(default)]
    pub connections_per_tenant: Option<u32>,
    /// sse: テナントごとに初期取得を 1 回にまとめる（キャッシュの stampede 対策）
    #[serde(default)]
    pub singleflight: bool,
    /// true: トランザクションで包み、ループの間ずっと接続を持つ
    #[serde(default)]
    pub tx: bool,
    pub steps: Vec<Step>,
}
fn one() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Slo {
    pub p95_ms: f64,
    pub error_rate: f64,
}
impl Default for Slo {
    fn default() -> Self {
        Slo {
            p95_ms: 100.0,
            error_rate: 0.001,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Pgbouncer,
    Proxysql,
}
/// アプリと DB の間にある Proxy。管理用の接続先を指定すると、Proxy の層の接続数を採取する
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProxyCfg {
    pub kind: ProxyKind,
    /// 管理コンソールの接続先（PgBouncer: host=… port=6432 user=… dbname=pgbouncer ／ ProxySQL: mysql://admin:admin@127.0.0.1:6032/）
    pub admin_dsn: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Writer,
    Reader,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChaosAction {
    /// DB 側からこのアプリの接続をすべて強制切断する（フェイルオーバー・再起動の簡易再現）
    KillConnections,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chaos {
    /// 計測開始（ウォームアップ終了）からの秒
    pub at_s: f64,
    pub action: ChaosAction,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub engine: EngineKind,
    /// 接続文字列。空なら環境変数 CAPLAB_DSN
    #[serde(default)]
    pub dsn: String,
    #[serde(default)]
    pub profile: Profile,
    pub tenants: u32,
    #[serde(default)]
    pub schema: Schema,
    #[serde(default)]
    pub queries: BTreeMap<String, QueryDef>,
    #[serde(default)]
    pub actors: Vec<Actor>,
    #[serde(default)]
    pub slo: Slo,
    /// 書き込みクエリの実行を許可する
    #[serde(default)]
    pub allow_writes: bool,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub tls: TlsMode,
    /// 障害注入（実行中に決めた時刻で起こす）
    #[serde(default)]
    pub chaos: Vec<Chaos>,
    /// Reader の接続先（空なら Writer と同じ）
    #[serde(default)]
    pub reader_dsn: String,
    /// アプリと DB の間の Proxy（管理コンソールから接続数を採取する）
    #[serde(default)]
    pub proxy: Option<ProxyCfg>,
    /// 切断などの接続エラーのとき、新しい接続で 1 回やり直す（database/sql の既定に近い）
    #[serde(default = "yes_")]
    pub retry_conn: bool,
}
fn yes_() -> bool {
    true
}

/// 校正結果（クエリ単体の CPU/回と平均返却行数）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Calib {
    pub cpu_ms: BTreeMap<String, f64>,
    pub avg_rows: BTreeMap<String, f64>,
    pub p50_ms: BTreeMap<String, f64>,
    /// 校正したときの同時数
    pub concurrency: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_multipliers() {
        let spike = Shape::Spike { at_s: 4.0, factor: 6.0, dur_s: 3.0 };
        assert_eq!((spike.mult(3.9, 10.0), spike.mult(4.0, 10.0), spike.mult(6.9, 10.0), spike.mult(7.0, 10.0)), (1.0, 6.0, 6.0, 1.0));
        let ramp = Shape::Ramp { from: 0.2, to: 2.2 };
        assert!((ramp.mult(5.0, 10.0) - 1.2).abs() < 1e-9);
        assert_eq!(ramp.peak(), 2.2);
    }
}
