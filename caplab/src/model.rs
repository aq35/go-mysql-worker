//! プロジェクト定義（1 プロジェクト = 1 つの JSON）。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// 負荷側が各クエリの後に待つ往復遅延（ms）。0 なら待たない（タイマーは約 1ms 単位）
    pub rtt_ms: f64,
}
impl Default for Profile {
    fn default() -> Self {
        Profile {
            vcpu: 2.0,
            max_connections: 200,
            safe_util: 0.6,
            reserve_vcpu: 0.1,
            rtt_ms: 0.0,
        }
    }
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
