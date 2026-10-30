# 容量実験 Web アプリ（caplab）設計

テーブル定義・クエリ・頻度を登録すると、**MySQL の CPU 負担と接続数を試算**し、同じ定義で**実際に負荷をかけて測り、試算と突き合わせる**アプリ。
きっかけは [tenant-cpu-isolation](tenant-cpu-isolation.md)（EXP-65）。あそこで手作業だった「ワーカー・Web・認証を混ぜた上限の見積り」をアプリにする。

## 目的と、やらないこと

| やる | やらない |
| --- | --- |
| 登録したワークロードの CPU 使用率・接続数の試算 | 本番 DB への接続（ガードで拒否） |
| 同じ定義の実行（負荷生成・計測） | 本番の SQL の自動収集（まず手で登録。収集は後段） |
| 試算と実測の差の表示（試算の信用度を知る） | DDL / DML の自動最適化提案 |
| テナント数の掃引と、限界点（膝）の自動探索 | MySQL 以外（PostgreSQL は後で `Driver` を足す） |

## EXP-65 から設計に入れた教訓

1. **1 クエリの CPU はクライアント数で変わる**（同じクエリが 0.42ms ↔ 0.77ms）。単一の「ms/回」を持たず、**同時接続数ごとの校正表**を持つ。
2. **試算は線形、実機は 88% 付近で曲がる**。試算は線形で出し、**膝の位置は実測の掃引から出す**。線形値だけを安全値として見せない。
3. **開ループ（到着率固定）で測る**。待ち行列を含めて遅延を測るため、遅延は「予定時刻」から数える（pgbench `-R` と同じ）。
4. **時刻をそろえると山ができる**。ワーカーの起動を `揃える / ずらす` で切り替えて比べられるようにする。
5. **測定条件を必ず残す**（`expkit.Env`）。絶対値はマシンで変わる、が前提。

## 構成

```
ブラウザ (SPA)  ──JSON──  cmd/caplab (Go, 単一バイナリ, 静的ファイル埋め込み)
                           ├─ model      プロジェクト定義（スキーマ・クエリ・アクター）
                           ├─ schema     DDL 取り込み・データ生成
                           ├─ calib      クエリ単体の校正（CPU/回 × 同時数）
                           ├─ estimate   試算（CPU・接続数・安全上限）
                           ├─ runner     負荷実行（開ループ・直列ステップ・テナント掃引）
                           ├─ dbstat     MySQL の状態採取（接続・実行行数）
                           ├─ cpusrc     CPU 採取のプラグイン（下記）
                           ├─ guard      実行先・SQL の安全確認
                           └─ store      プロジェクト JSON と実行結果の保存
                                │
                                ▼
                       実験用 MySQL（ラボ専用。例: docker compose の mysql:8.0 に --cpus=2）
```

- 言語は Go。既存の `internal/expkit`（`Latency`・`Sampler`・`Env`・`Recorder`）と `internal/lint`（テナント条件の検査）を再利用する。
- 新規パッケージは `internal/caplab/…`、エントリは `cmd/caplab`。フロントは素の HTML + 小さな JS（ビルド不要）にして `embed` で同梱する。
- 定義は **1 プロジェクト = 1 つの JSON ファイル**（git に入れられる）。実行結果は `runs/<時刻>/` に JSON + Markdown で保存する。DB は持たない。

## データモデル（プロジェクト JSON）

```jsonc
{
  "name": "site-monitor",
  "profiles": [                       // DB スペックの候補。画面で切り替え・並べて比較できる
    {"name": "2vcpu", "vcpu": 2, "memory_gb": 8,  "max_connections": 200, "dsn_env": "CAPLAB_DSN_2"},
    {"name": "4vcpu", "vcpu": 4, "memory_gb": 16, "max_connections": 400, "dsn_env": "CAPLAB_DSN_4"}
  ],
  "active_profile": "2vcpu",
  "tenants": {"count": 300, "boundary_column": "tenant_id"},
  "schema": {
    "ddl": ["CREATE TABLE sites(...)", "..."],
    "tables": [{"name": "devices", "rows_per_tenant": 100,
                "columns": {"status": {"gen": "enum", "values": ["ok","ng"]},
                            "id": {"gen": "seq"}}}]
  },
  "queries": {
    "poll_devices": {"sql": "SELECT ... WHERE tenant_id = :tenant AND id > :after",
                      "params": {"tenant": "tenant", "after": {"gen": "int", "min": 0, "max": 100}}}
  },
  "actors": [
    {"kind": "worker", "name": "sync-1s", "every": "1s", "per": "tenant",
     "stagger": true, "pool": {"max_open": 4, "max_idle": 4},
     "steps": [{"query": "poll_devices"}, {"query": "poll_devices"}, ...]},   // 直列 6 回なら 6 行
    {"kind": "web", "name": "api-list", "rate": {"per": "tenant", "qps": 1.2},
     "pool": {"max_open": 20}, "steps": [{"query": "auth_session"}, {"query": "list_devices"}]}
  ]
}
```

- **ステップは順序つきの配列**。「1 秒に直列 6 回」は 6 要素（同じクエリを繰り返してもよい。`{"query": "x", "repeat": 6}` の省略記法も許す）。
- **Worker**は `every`（間隔）× `per`（`tenant` ごと / `global`）で頻度を決める。`instances`（同じワーカーを何本動かすか）も持つ。
- **Web** は `rate`（`per: tenant | user`、`qps`）。`per: user` のときは `users_per_tenant` を掛ける。API ごとに 1 アクターを登録する。
- 認証クエリは、ふつうのクエリとして Web アクターのステップの先頭に置く（キャッシュ率は `cache_hit` で表す。例: `{"query":"auth","cache_hit":0.98}`）。
- **SSE** は `connections_per_tenant` と、接続時に流す初期クエリ（`on_connect` ステップ）だけを持つ。配信は DB を引かない前提（hub 方式）。
- クエリの `params` はジェネレータ名か、`tenant`（現在のテナント ID）、`sample(table.column)`（実データから抽出）。

## DB スペックの切り替え（2 vCPU → 4 vCPU など）

スペックは**プロジェクトに固定せず `profiles` として持ち**、画面の切り替えで試算・実行の前提を変える。同じワークロードを 2 つ以上のプロファイルで並べて比較できる（「4 vCPU にすると上限は何テナントか」）。

| 項目 | 切り替えで変わるか | 扱い |
| --- | --- | --- |
| `vcpu` | 変わる | 試算の分母。実行時は CPU 上限（cgroup quota / `docker --cpus`）の指示にも使う |
| `max_connections` | 変わる | 接続の予算（予約を引く）。RDS ならメモリから決まる既定式を選べる |
| `memory_gb`（バッファプール） | 変わる | **試算では使わず、警告のみ**（データ量 > バッファプールなら「ディスク読みが出る」と注意）。効果の見積りは未対応 |
| `cpu_ms/回`（校正表） | **ほぼ変わらないが保証されない** | プロファイルごとに持つ。別プロファイルの校正値を流用するときは「流用」と表示する |
| 悪化開始・崩壊の使用率（膝） | **変わりうる** | プロファイルごとに掃引で測る。**無ければ既定値（60/88/95%）に「2 vCPU の実測の流用」と注記** |
| 安全使用率・予約 vCPU | 変わる | プロファイルごとに設定。予約は固定値（0.1 vCPU）なので、vCPU が増えるほど割合は小さくなる |

- **試算は vCPU に対して線形**に出す（CPU 合計 ÷ vCPU）。ただし 4 vCPU で 2 倍の上限になるとは**限らない**。
  実際には、同時実行が増えるぶんロック競合・コンテキストスイッチ・バッファプールの競合で CPU/回が増える
  可能性がある（EXP-65 で同時数により 0.42 ↔ 0.77ms と変わった）。そのため、**試算の画面は「線形の値」と
  「そのプロファイルで校正・掃引した値」を区別して表示**し、後者が無いプロファイルには「未検証」を付ける。
- **実行は、実際に 4 vCPU の DB に向けて測る**のが唯一の確認方法。ローカルなら `docker --cpus=4`（または cgroup quota 4000m）、
  RDS なら該当インスタンスクラスの DB を指す。負荷側は DB と別のコア / ホストにする（4 vCPU の DB を測るなら、負荷側は 4 コア以上要る場合がある。
  アプリがスケジュール遅れを見て「負荷側が詰まっている」と警告する）。
- **プロファイル間の比較画面**: 同じテナント数で、vCPU ごとの使用率・p95・安全上限を並べる。
  「2 vCPU の安全上限 → 4 vCPU の安全上限（試算 / 実測）」の比が、**線形からのずれ**としてそのまま見える。
- 将来 MySQL 以外・Aurora などを足すときも、`profiles` に `engine` と CPU 採取方式（`cpusrc`）を持たせるだけで済むようにする。

## 試算（`estimate`）— 実行せずに出す

入力は上の定義 + 有効なプロファイル + 校正表（下記）。出力:

**1. クエリごとの実行頻度**

```
qps(q) = Σ_actor  rate(actor) × tenants × (actor.steps の中の q の回数) × (1 − cache_hit)
rate(worker) = instances / every          rate(web) = qps / テナント（user なら × users）
```

**2. CPU**

```
cpu_vcpu(q)  = qps(q) × cpu_ms(q, c) / 1000          c = 想定同時接続数（接続数の試算から引く）
cpu_util     = Σ cpu_vcpu(q) / profile.vcpu      // 有効なプロファイルの vcpu
```

`cpu_ms(q, c)` は校正表から線形補間する（**クライアント数で変わる**ため。教訓 1）。校正が無いクエリは「未校正」と表示し、合計は幅（最小〜最大）で出す。

**3. 接続数**（リトルの法則 + プール上限）

```
同時実行(actor) ≈ min( qps(actor) × latency_wall(actor), pool.max_open × インスタンス数 )
接続合計 = Σ pool.max_open × インスタンス数   // 上限側（張りっぱなしを含む）
        vs  max_connections、予約（migration・監視など）を引いた予算
```

「上限側の接続」と「同時実行（Threads_running 相当）」を別々に出す。接続が足りなくなるのは前者、CPU が効くのは後者。

**4. 安全上限**

```
安全使用率 = 設定値（既定 60%）、非テナント処理の予約 = 設定値（既定 0.1 vCPU）
安全上限テナント数 = (vcpu × 安全使用率 − 予約) / (1 テナントあたりの cpu_vcpu)
```

結果画面は、内訳（アクター別・クエリ別の積み上げ）、使用率バー（安全 / 悪化開始 / 崩壊の目盛り。**目盛りの位置は実測の掃引結果があればそれを使い、無ければ既定の 60/88/95% と注記**）、安全上限を出す。

## 校正（`calib`）— クエリ単体のコストを測る

- 各クエリを、同時数 c ∈ {1, 4, 16, 32, 64}（設定可）で、**1 点あたり 5〜10 秒**、開ループで流し、`cpu_ms/回` と `p50/p95` を記録する。
- `EXPLAIN ANALYZE` の結果（読み取り行数・使ったインデックス）を併記し、**全表走査・テナント条件なし**を警告する（`internal/lint` を呼ぶ）。
- 校正結果はプロジェクトに保存し、スキーマやデータ量を変えたら「古い」と表示する。

## 実行（`runner`）

- 各アクターを、**テナント × インスタンス**ごとのスケジューラとして動かす。ステップは**直列**（前の完了を待つ）、起動は**開ループ**（予定時刻で発火し、遅延は予定時刻から数える）。
- `stagger: true` は起動時刻をテナントごとにずらす（一様分布）。`false` は同時起動。**同じ定義で両方を比べる**ボタンを付ける。
- 実行モード:
  - `steady`: 指定テナント数で一定時間（ウォームアップ除外）。
  - `sweep`: テナント数を段階的に上げ（例 100 → 1000）、**p95 が閾値を超える、または CPU が上限を超える最初の点を「膝」として記録して止める**。
  - `calibrate`: 上記の校正。
- 計測するもの（1 秒ごと）: クエリ別 qps・p50/p95/p99・エラー、`Threads_connected` / `Threads_running`、`Innodb_rows_read` の差分、DB の CPU（`cpusrc`）、アプリ側のスケジュール遅れ（これが大きいと負荷側が詰まっている印）。
- **負荷側の CPU を DB と分ける**（別コアに固定、または別ホスト）。同じホストなら、アプリがその旨を警告する（EXP-65 では DB をコア 0,1・負荷をコア 2,3 に固定した）。

## CPU の採取（`cpusrc` プラグイン）

| 方式 | 使える場面 | 状態 |
| --- | --- | --- |
| `cgroup` | 同一ホストで cgroup（v1 `cpuacct.usage` / v2 `cpu.stat`）が読める | EXP-65 で PostgreSQL に対して使用済み。MySQL でも同じ |
| `docker` | `docker stats` / cgroup 経由 | 未実装（環境に docker が無く未確認） |
| `performance_schema` | 接続だけで取れる。8.0.28+ の `events_statements_*` の `CPU_TIME` 列 | **要検証**（列の有無とバージョン、要約テーブルに無い点の確認が必要） |
| `cloudwatch` | RDS / Aurora の `CPUUtilization`（粒度 1 分、最大 1 秒の拡張モニタリング） | 後段。粒度が粗く校正には不向き |
| `manual` | ユーザーが値を入力 | フォールバック |

どの方式かは、結果に必ず記録する。方式によって「DB の CPU」の意味（ホスト全体 / mysqld のみ）が違うため。

## ガード（`guard`）

- 実行先は**許可リスト**（既定 `127.0.0.1` と `*.lab.*` など）。許可外のホストは実行しない。DSN は環境変数から読み、プロジェクト JSON には書かない。
- 使うスキーマ名は `caplab_` で始まるものに限る。それ以外へのクエリは拒否する。
- SQL は 1 文のみ（複数文の拒否）、既定は `SELECT` のみ。`INSERT/UPDATE/DELETE` はプロジェクトごとに明示的に許可し、実行は専用スキーマ内のみ。DDL は取り込み時の 1 回だけで、実行中は不可。
- 上限: 最大 qps・最大同時接続・最大時間をプロジェクトで設定。画面に常に「停止」を出し、停止時はすべて即時にキャンセルする。
- `tenant_id` 条件のないクエリは**警告**（実行は可能。EXP-58 の事故型。[tenant-scope](tenant-scope.md)）。

## 画面

| 画面 | 内容 |
| --- | --- |
| 1 スキーマ | DDL を貼る / ファイルを読む。テーブルごとの行数 / テナント、列ごとのジェネレータ。「データを作る」ボタン |
| 2 クエリ | SQL・パラメータ。「校正」で CPU/回と `EXPLAIN` を表示。テナント条件の警告 |
| 3 ワークロード | Worker / Web / SSE をフォームで追加。ステップ（直列）の並び替え、間隔・頻度・プール |
| 4 試算 | テナント数のスライダ。使用率バー、アクター別・クエリ別の積み上げ、接続数、安全上限。変更が即時に反映 |
| 5 実行 | モード（steady / sweep）、開始 / 停止、ライブグラフ（qps・p95・CPU・接続） |
| 6 比較 | 試算と実測の並び（差の %）、過去の実行の一覧。**試算が実測から何 % ずれたか**を常に見せる |

## API（JSON）

```
GET/PUT  /api/project                    プロジェクト定義の取得・保存
POST     /api/schema/apply               DDL の適用（ラボ DB のみ）
POST     /api/data/generate              データ生成
POST     /api/calibrate                  校正の開始 → run_id
GET      /api/estimate?tenants=300       試算（実行なし）
POST     /api/runs  {mode, tenants, …}   実行開始 → run_id
GET      /api/runs/{id}/stream           進捗（Server-Sent Events）
POST     /api/runs/{id}/stop             停止
GET      /api/runs, /api/runs/{id}       履歴・結果
```

## テスト方針

- **試算の受け入れテスト**: EXP-65 の記録（`docs/results/tenant-cpu-lab/*.tsv`、1 クエリ 0.74〜0.77ms、2,000 テナントで使用率 75〜80%）から作ったプロジェクトを試算して、実測の ±15% 以内に収まる。
- 純粋関数（試算・リトルの法則・補間）は表駆動テスト。DB なしで動く。
- 実行系は実 MySQL に対する統合テスト（既存の `mysqltest` を使う）。ガードは「拒否されること」を必ず検証する。
- UI は Playwright で、定義 → 試算 → 小さな実行 → 比較の一連を 1 本通す。

## 実装の順序

| 段階 | 内容 | 検証できること |
| --- | --- | --- |
| M1 | `model`（`profiles` を含む）+ `estimate`（DB なし）+ 試算 API + 画面 3・4 | 手入力した校正値から試算が出る。EXP-65 の再現。vCPU の切り替えで結果が変わる |
| M2 | `schema` + データ生成 + `calib` + 画面 1・2 | クエリの CPU/回の校正表 |
| M3 | `runner`（steady）+ `cpusrc(cgroup)` + `dbstat` | 試算と実測の突き合わせ（画面 5・6） |
| M4 | sweep と膝の自動探索、stagger の比較、プロファイル間の比較 | 限界点を自動で出す。4 vCPU が線形からどれだけずれるか |
| M5 | `performance_schema` / CloudWatch の CPU 採取、SSE アクター | 環境ごとの採取方法 |

## 未確認・要判断

- **MySQL をこの環境で動かせていない**（docker も mysqld も無い）。EXP-65 の実測はすべて PostgreSQL。MySQL での CPU/回、`CPU_TIME` 列の有無、cgroup での採取は**実装時に最初に確認する**。
- 実行先 MySQL をどこで動かすか（ローカルの docker compose を想定。RDS を直接測る場合は CPU の採取方法が変わる）。
- 試算が実測から外れる要因（ロック競合・バッファプールのヒット率・データ分布の偏り）は、線形モデルでは表せない。**差が大きいときは「試算を信用しない」ことを画面が示す**設計にする。
- 認証の実クエリ・Web の実クエリは、ユーザーが登録するまで仮のものしか無い。
- **4 vCPU の実測はまだ無い。** 4 vCPU での膝の位置・CPU/回は、実際に 4 vCPU の DB を測るまで分からない（線形の試算は出せる）。
