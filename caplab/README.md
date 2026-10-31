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

DB と負荷生成を同じホストで動かすなら、**別のコアに分ける**（例 `taskset -c 2,3 caplab`、DB は `0,1`）。負荷側の CPU が高いと測定が信用できません（画面に出ます）。

## 使い方

1. **① 設定**: 親→子→孫のテーブル（カラム数・サイズ 小/中/大・件数）、接続先と条件、「ざっくりモード」でワークロード（ワーカー・Web（認証つき）・重いバッチ）を作る。
2. **② 試算**: テナント数のスライダで、CPU・接続数・安全上限を見る。RTT を変えたときの必要な接続数も並ぶ。
3. **③ 実行とメトリクス**: 「かんたん実行」（スキーマ投入 → 校正 → 実行）。飽和ボード・チャート・結論カード・試算と実測の差。
4. **④ 履歴**: 過去の実行を開く。

## 安全のためのガード

- 接続先がローカル以外なら拒否（`CAPLAB_ALLOW_REMOTE=1` で解除）。
- PostgreSQL は専用スキーマ `caplab` の中だけで動く（`search_path` を固定。既存の表に触れない）。MySQL は `caplab` で始まるデータベース名のみ。
- 書き込みクエリは既定で禁止（プロジェクトの `allow_writes`）。DB の耐久性設定が緩い（`fsync` / `synchronous_commit` / `innodb_flush_log_at_trx_commit`）と、書き込みを含む結果に注記が付く。
- スキーマ投入は `DROP TABLE IF EXISTS` を実行する（対象はプロジェクトに書いた表と `sessions` のみ）。

## 検証済み・未検証

| | 状態 |
| --- | --- |
| PostgreSQL 16 での通し（スキーマ投入 → 校正 → 実行 → 結論、UI 操作、プール枯渇の検出） | **検証済み**（この環境。cgroup で CPU を採取） |
| 試算の単体テスト（EXP-65 の再現、N+1 の回数、認証キャッシュ、プール枯渇の警告） | `cargo test` で 5 件 |
| **MySQL** | **未検証**。ドライバと `SHOW GLOBAL STATUS` の取り込みは実装・コンパイル済みだが、この環境に mysqld が無く一度も動かしていない |
| RDS / Aurora への接続（TLS、CloudWatch からの CPU 採取） | **未実装**（TLS 無しのドライバ設定。CPU は「なし」になる） |
| 書き込み負荷の実行 | 実装済みだが**未検証**（`allow_writes` を有効にした実行は試していない） |
| プリペアドステートメント（`protocol: prepared`） | **未実装**（テキストプロトコルのみ。Proxy のピン留めの実験には足りない） |
| ネットワーク遅延 | クエリごとに RTT を待つ簡易版（タイマーの刻み約 1ms。短い遅延は不正確）。中間サーバ / `tc netem` は未実装 |
| DDL の取り込み、`pg_stat_statements` / `performance_schema` からのクエリ取り込み | **未実装**（かんたんスキーマのみ） |
| 障害時の挙動（フェイルオーバー・再接続の嵐）、ソーク試験、複数回の繰り返しとばらつき | **未実装** |
| 2 つの実行の重ね合わせ | **未実装**（履歴から 1 つずつ開く） |

## 既知の偏り

- **試算の CPU は、実測の約半分に出る傾向がある**（この環境での 2 回の実測: +107% / +133%。校正は 8 同時の単純な繰り返しで、実行時の接続数・クエリの混ざり方・DB のバックグラウンドの CPU が入らないため、と推測。**未切り分け**）。画面の「試算と実測の差」で毎回確認できる。
- 負荷は同一ホスト・127.0.0.1。実環境のネットワーク遅延は含まれない。
