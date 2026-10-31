# caplab — DB 容量実験アプリ（MySQL / PostgreSQL）

テーブル・クエリ・頻度を登録して、**DB の CPU・接続数を試算**し、同じ定義で**実際に負荷をかけて測り、試算と突き合わせる**。
設計は [docs/capacity-lab-design.md](../docs/capacity-lab-design.md)、きっかけは [docs/tenant-cpu-isolation.md](../docs/tenant-cpu-isolation.md)（EXP-65）。

## 起動

```bash
cd caplab
export CAPLAB_DSN='host=127.0.0.1 port=5432 user=postgres dbname=postgres'   # PostgreSQL（専用スキーマ caplab だけを使う）
# MySQL: export CAPLAB_DSN='mysql://user:pass@127.0.0.1:3306/caplab_lab'      # データベース名は caplab で始める
export CAPLAB_CPU_CGROUP=tcl/cl      # 任意。DB の CPU を測る cgroup（v1 cpuacct / v2 cpu.stat）。無ければ同一ホストの DB プロセスから簡易採取
cargo run --release                  # http://127.0.0.1:8080
```

| 環境変数 | 意味 |
| --- | --- |
| `CAPLAB_DSN` | 接続先（プロジェクトの `dsn` が優先） |
| `CAPLAB_DIR` | 定義・校正・実行結果の保存先（既定 `./caplab-data`） |
| `CAPLAB_ADDR` | 待ち受け（既定 `127.0.0.1:8080`） |
| `CAPLAB_CPU_CGROUP` | DB の CPU を取る cgroup のパス（例 `tcl/cl`） |
| `CAPLAB_ALLOW_REMOTE=1` | ローカル以外への接続を許可（**実験用 DB のみ。本番には向けない**） |
| `CAPLAB_TLS_CA` | 接続の暗号化（検証あり）で使う CA の PEM（RDS のグローバルバンドルなど） |
| `CAPLAB_ENGINE=mysql` | `caplab probe` のとき MySQL として扱う |

DB と負荷生成を同じホストで動かすなら、**別のコアに分ける**（例 `taskset -c 2,3 caplab`、DB は `0,1`）。負荷側の CPU が高いと測定が信用できません（画面に出ます）。

## 使い方

1. **① 設定**: 親→子→孫のテーブル（カラム数・サイズ 小/中/大・件数）、接続先と条件、「ざっくりモード」でワークロード（ワーカー・Web（認証つき）・重いバッチ）を作る。
2. **② 試算**: テナント数のスライダで、CPU・接続数・安全上限を見る。RTT を変えたときの必要な接続数も並ぶ。
3. **③ 実行とメトリクス**: 「かんたん実行」（スキーマ投入 → 校正 → 実行）。飽和ボード・チャート・結論カード・試算と実測の差。
4. **④ 履歴**: 過去の実行を開く。掃引・繰り返しの結果、**2 つの実行の重ね合わせ**。

追加の操作:

- **② 試算 →「診断」**: EXPLAIN（全件走査）、テナント条件の lint、データ量と DB のメモリキャッシュの比較。
- **③ 実行 →「テナント数の掃引」「N 回繰り返す」**: 限界（膝）と安全上限、測定のばらつき。負荷側が先に詰まった点は判定から除く。
- **① 設定 →「詳細な条件」**: クエリの送り方（テキスト / プリペア / 毎回プリペア）、TLS、遅延の演出（中間サーバ）、Proxy（PgBouncer / ProxySQL）の観察、Reader、費用。
- **① 設定 →「取り込み」**: `CREATE TABLE`、貼り付けたログ、`pg_stat_statements` / `performance_schema` から。
- **CLI**: `caplab probe [--save]`（ECS のコンテナ内などで接続確立と `SELECT 1` の往復を測る）。

## 安全のためのガード

- 接続先がローカル以外なら拒否（`CAPLAB_ALLOW_REMOTE=1` で解除）。
- PostgreSQL は専用スキーマ `caplab` の中だけで動く（`search_path` を固定。既存の表に触れない）。MySQL は `caplab` で始まるデータベース名のみ。
- 書き込みクエリは既定で禁止（プロジェクトの `allow_writes`）。DB の耐久性設定が緩い（`fsync` / `synchronous_commit` / `innodb_flush_log_at_trx_commit`）と、書き込みを含む結果に注記が付く。
- スキーマ投入は `DROP TABLE IF EXISTS` を実行する（対象はプロジェクトに書いた表と `sessions` のみ）。

## 検証済み・未検証

検証は、この環境のローカルな **PostgreSQL 16.15** と **MySQL 8.0.46**（どちらも cgroup で 2 vCPU 相当に制限、CPU は cgroup から採取）で行った。数値と手順は [docs/caplab-verification.md](../docs/caplab-verification.md)。

| | PostgreSQL | MySQL |
| --- | --- | --- |
| 通し（スキーマ投入 → 校正 → 実行 → 結論 → 画面） | ✓ | ✓ |
| 送り方（テキスト / プリペア / 毎回プリペア） | ✓ | ✓ |
| 書き込みの負荷（耐久性設定の警告つき） | ✓（`fsync=off` の環境） | ✓（`flush_log_at_trx_commit=1`） |
| 障害注入（接続の強制切断・回復時間・再試行） | ✓ | ✓ |
| RTT 計測（probe）と遅延の演出（中間サーバ） | ✓ | ✓ |
| ホットロウ・テナントの偏り・スパイク・SSE の一斉接続 | ✓ | ✓ |
| EXPLAIN・lint・データ量とキャッシュの比較 | ✓ | ✓ |
| クエリの取り込み（テキスト・統計ビュー） | ✓ `pg_stat_statements` | ✓ `performance_schema` |
| TLS（暗号化のみ / 検証あり） | ✓（自己署名: 暗号化できる・検証は拒否） | ✓（同左） |
| テナント数の掃引・繰り返し・ソーク（ドリフト検出） | ✓（ソークは 36 秒まで） | 未実施 |
| Proxy の 3 層観察 | ✓ PgBouncer | ProxySQL は**実装のみ・未検証** |
| DDL の取り込み | ✓（PostgreSQL の書き方） | MySQL の書き方は未検証 |
| 画面の操作（Playwright） | ✓ | 未実施（API は上記のとおり） |

テスト: `cargo test`（11 件。試算・取り込み・DSN の書き換え・負荷の形）。

### まだできていないこと・限界

- **RDS / Aurora の実環境**: TLS の検証は自己署名のローカルのみ（RDS の CA での検証は未確認）。CloudWatch の API から直接取る機能は無い（CSV の取り込みで代用）。**RDS Proxy 固有のピン留めは再現できない**（PgBouncer は規則が違う）。**ProxySQL は未検証**。
- **フェイルオーバー**: 「接続の強制切断」までで、本物のフェイルオーバー（書き込み先の切り替え・レプリカの遅れ）は試していない。Reader は同じ DB を指しただけで、実際のレプリカは試していない。
- **長時間のソーク**: 36 秒まで。数時間の実行（メモリ・肥大・autovacuum / purge の遅れ）は未検証。
- **`tc netem`**: 使っていない（この環境に無い）。遅延は内蔵の中間サーバ。**精度は設定 RTT に +0.13〜0.46ms の上乗せ**（0.2〜5ms の範囲で測定）。
- **MySQL で多数のループがプールを待つ形（SSE の一斉接続など）が、PostgreSQL の約 17〜20 倍遅い**（3,000 ループ・プール 10 で p50 3.1 秒 vs 0.18 秒）。DB の実行時間は 0.5〜0.7ms、DB・負荷側の CPU は余っている。**原因は未特定**。TCP_NODELAY の明示・Unix ソケット・ランタイムのスレッド数・アイドル後のサーバ側の起床の遅さ（どちらのエンジンも同程度）は、いずれも原因ではなかった。MySQL のプール待ちが大きい結果は、負荷生成側（`mysql_async` と本アプリのプールの組み合わせ）の可能性を疑って読むこと。
- **試算の CPU が実測の約半分に出る傾向**（PostgreSQL で +107% / +133%、後の実行では +11% など幅がある）。画面の「試算と実測の差」で毎回確認できる。
- プリペアドは `$n::integer` / `?` に整数だけを束縛する（文字列・日時のパラメータは未対応）。
- 取り込んだクエリの値は 1〜1000 の乱数。DDL の取り込みは、元のスキーマそのものでなくかんたんスキーマへの近似（インデックス・制約・方言固有の型は反映されない）。
- DB が負荷側と同じホストの前提。DB と負荷生成は別のコアに分けること（画面に負荷側の CPU を出す）。
