# Go + MySQL 常時稼働ワーカー — 設計ガイドと、その検証コード

[`調査`](調査) は、外部サービスから状態を受け取って MySQL に保存し続けるワーカーの設計ガイド。
このリポジトリは、**そこに書いてあることを実際に動かして確かめるためのコード**を足したもの。

- 資料の主張（「変化がなければ書かない」「沈黙からオフラインと判定しない」など）は、
  すべて **動くコードとテスト** になっている
- 数値はすべて **この環境で測り直した**。条件は [`docs/measurements.md`](docs/measurements.md)
- 資料 §9 の「未検証の項目」は、**実際の MySQL 8.0 に聞いて確認した**（`internal/mysqlfacts/`）
- **リポジトリ層（DB アクセス層）の設計**は [`docs/repository-layer.md`](docs/repository-layer.md)。
  マルチテナントのテナント分離、誤更新の防止、性能を崩さない書き方を、実験して決めたもの
- **排他制御（GET_LOCK は要るのか・代わりは何か）**は [`docs/locking.md`](docs/locking.md)
- **異常系の実験**（crash / 担当の交代 / 終了 / マイグレーション）:
  [`docs/experiments.md`](docs/experiments.md)（実験基盤）、
  [`crash-effects.md`](docs/crash-effects.md)、[`fencing.md`](docs/fencing.md)、
  [`shutdown.md`](docs/shutdown.md)、[`migration-crash.md`](docs/migration-crash.md)
- 他プロジェクトのレビューに使う入口は 2つ
  - [`.claude/skills/go-mysql-worker-review/`](.claude/skills/go-mysql-worker-review/SKILL.md) — 常時稼働ワーカー
  - [`.claude/skills/go-mysql-repository-review/`](.claude/skills/go-mysql-repository-review/SKILL.md) — リポジトリ層

## 試す

```bash
# DB を使わない項目だけなら、これだけで動く
go test ./... -v

# MySQL 8.0 を用意して、全部動かす
eval "$(./scripts/mysql-up.sh --export)"
./scripts/run-all.sh              # 結果は docs/results/ に残る

# 設計をひとまとめに動かすデモ（外部サービスは内蔵の代役を使う）
go run ./cmd/worker -tenants 2 -robots 100 -duration 50s

# §4.1 の表（1000 → 10 → 5 tx/秒）を実際に出す
go run ./cmd/loadsim -rate 1000 -duration 10s -robots 1000 -change-rate 0.01

# リポジトリ層: 事故を実際に起こす実験 → 層が止めることの確認
go test ./internal/repo/ -run TestExperiment_ -v
go test ./internal/repo/... -v

# 排他制御（GET_LOCK の性質と代替）
go test ./internal/repo/ -run TestExperimentLock_ -v

# 異常系の実験（それぞれ docs/results/exp-N/ に測定条件つきの結果を残す）
go test ./internal/effectlab/  -run TestEXP1 -v   # 外部 effect 途中の SIGKILL
go test ./internal/fencelab/   -run TestEXP2 -v   # lease / fencing / 時計のずれ
go test ./internal/shutdownlab/ -run TestEXP3 -v  # graceful shutdown
go test ./internal/repo/       -run TestEXP6 -v   # マイグレーション途中の crash
```

`MYSQL_DSN` が未設定のときは、DB を使うテストは skip される（CI で壊れない）。

## 実験の全体像（EXP-1〜17）と設計判断

事故を再現し、防止策で止まることを before/after で測り、結果を `docs/results/exp-N/` に実行 SHA つきで残している。一覧と各文書は [`docs/experiments.md`](docs/experiments.md)。

| 実験 | 何を確かめたか |
| --- | --- |
| EXP-1〜3 | 外部 effect crash・lease/fencing・graceful shutdown |
| EXP-4〜7 | backpressure・接続プール飽和・migration crash・実行計画の偏り |
| EXP-8〜9 | SQL 検査の fuzz・`go/analysis` による保守性検査 |
| EXP-10〜11 | SQLite 移植契約・バックアップ/復元/破損 |
| EXP-12〜14 | ポーリング頻度・DB資格情報のローテーション・fan-out を畳む |
| EXP-15〜17 | テーブル分割と競合・担当テナント数の上限・適応的バックオフ |

**設計判断（30 テナント・接続 1000 を具体に）**: [`docs/web-worker-split.md`](docs/web-worker-split.md)
（Worker/Web 分離・統合ワーカー・接続予算）、[`docs/scheduling.md`](docs/scheduling.md)（スケジュール/命令/実績とワーカー頻度）、
[`docs/fanout.md`](docs/fanout.md)（テナント分離を保って fan-out を畳む）、[`docs/table-split.md`](docs/table-split.md)（テーブル分割と競合）。

## どこに何があるか

| 場所 | 中身 | 資料の対応 |
| --- | --- | --- |
| `internal/model/` | 比較対象の型（`State`）。時刻も浮動小数も入れない | §4.2 / §6.3 |
| `internal/worker/` | 「変化がなければ書かない」記憶とバッファ。DB もネットワークも触らない | §3 / §4.2 / §4.3 |
| `internal/jsonx/` | 要る項目だけの構造体、配列の1件ずつ読み、未知項目の記録 | §6 |
| `internal/store/` | 冪等なバッチ書き込み、スキーマ、プール設定の検査 | §2.7 / §4 / §5 |
| `internal/lease/` | 1テナント1担当（リース） | §2.8 |
| `internal/app/` | 3本のループ（A 全件同期 / B 鮮度チェック / C 受信）と起動順序 | §2 |
| `internal/fakesvc/` | 外部サービスの代役。沈黙・API 停止・Pong 無応答を意図的に起こせる | §2.4 / §2.5 / §2.6 |
| `internal/mysqlfacts/` | MySQL の挙動確認（§9 の未検証項目） | §9 |
| `internal/repo/` | **リポジトリ層**。テナント束縛・誤更新の防止・キーセットページ送り・実行計画の検査 | [repository-layer.md](docs/repository-layer.md) |
| `internal/repo/repotest/` | 実行計画と問い合わせ回数をテストで縛る道具（他プロジェクトへ移植可） | 同上 |
| `internal/repo/` ほか | リポジトリ層（テナント束縛・誤更新防止・ページ送り・実行計画） | [repository-layer.md](docs/repository-layer.md) |
| `internal/lint/` + `cmd/sqllint/` | 保守性の静的検査（EXP-9）。層をまたぐ import の禁止（layerimport）も | [static-analysis.md](docs/static-analysis.md) |
| `internal/kascontract/` | ドライバ戻り値を domain 結果へ正規化。両 engine で同一（EXP-10） | [kas-adoption.md](docs/kas-adoption.md) |
| `internal/config/` | 環境変数を1箇所で。必須は fail-fast・任意は既定・秘密は期限つき | [secrets.md](docs/secrets.md) |
| `internal/tenantcache/` `secretcache/` | テナント隔離メモリ／期限つき秘密（singleflight） | [secrets.md](docs/secrets.md) |
| `internal/poolbudget/` | 接続予算（コンテナ×プール ≤ 上限）と起動時ガード | [pool-saturation.md](docs/pool-saturation.md) |
| `internal/tenantworker/` | 統合ワーカー（lease×fanout×backoff×fence×独立観測） | [web-worker-split.md](docs/web-worker-split.md) |
| `internal/structfilllab/` | 構造体のセット漏れ（作り方4種の比較・`FillNonZero` でラウンドトリップ） | [struct-fill.md](docs/struct-fill.md) |
| `internal/appx/` | app 層の小物（Ternary 等・samber/lo 基盤） | [samber-io.md](docs/samber-io.md) |
| `cmd/worker/` | 状態同期のデモ。途中で事故を起こす。接続予算ガード付き | — |
| `cmd/dispatcher/` | 統合ワーカーを単体で動かす（命令の配信） | [web-worker-split.md](docs/web-worker-split.md) |
| `cmd/web/` | Web プロセス（Worker と別・別プール・予算ガード・graceful） | [web-worker-split.md](docs/web-worker-split.md) |
| `cmd/loadsim/` | 書き方3種のトランザクション数を比較 | §4.1 |

## 何が確かめられたか（要点）

測定条件は [`docs/measurements.md`](docs/measurements.md)。**絶対値は環境で変わる。桁の関係が要点。**

| 主張 | 実測 |
| --- | --- |
| 変化がなければ書かない（§4.1） | 1,000件/秒・変化率1% で **1000 → 10.7 tx/秒**。まとめ書きで **4.3 tx/秒** |
| 素直に1件ずつ書くと追いつかない | 1,000件/秒を10秒流して **6.1秒の遅延**（他の2方式は遅延 20ms 以下） |
| まとめて書く（§4.3） | 100行の書き込み: 1件ずつ **114ms** → 1トランザクション **21ms** → 1文にまとめて **4.4ms** |
| `map[string]any` を使わない（§6.1） | 1,000件の解析で メモリ **15倍**・割り当て **22倍** の差 |
| 大きい配列は1件ずつ読む（§6.2） | 受信時のメモリ **2,724KB → 107KB**（25倍） |
| 整数型の列挙は「速いから」ではない（§8） | 比較 1回 **19.8ns（uint8）vs 21.7ns（string）**。差は誤差 |
| 1対象あたり約200バイト（§3.3） | 実測 **257〜356 バイト**。10万件で **約 25MB**（見積もりより大きいが桁は同じ） |
| `MaxIdleConns` の既定値 2（§5.2） | 300クエリで新規接続 **205本（既定）vs 10本（= MaxOpenConns）** |
| ソートしないとデッドロックする（§4.3） | すれ違い順で **再現**。主キー順に揃えると起きない |
| 履歴は `DELETE` ではなくパーティション（§4.6） | 5万行の削除: `DROP PARTITION` **14ms** vs `DELETE` **214ms**（15倍） |
| 長いトランザクションは undo を伸ばす（§4.6） | History list length **179 → 2000**、コミット後 **0** |
| 1往復 ≒ 1ms（§1） | 同一ホストで `SELECT 1` **85µs** / 主キー1件 **146µs** / 1件 UPDATE **1.11ms** |

## リポジトリ層で確かめたこと（要点）

詳細と再現手順は [`docs/repository-layer.md`](docs/repository-layer.md)。

| 主張 | 実測 |
| --- | --- |
| 読んで書くと更新が消える | 8並列×25回の read-modify-write で **200回中 175回が消えた**（エラーは出ない） |
| WHERE の書き間違いは静かに通る | 1行のつもりの UPDATE が **5行**に当たり、そのままコミットされる |
| tenant_id 忘れは他テナントを壊す | 他テナントの行まで書き換わった。MySQL はエラーを返さない |
| 主キーは `(tenant_id, id)` | テナント単位の読み出しが **2.6倍** 速い |
| ランダム UUID を主キーにしない | 表サイズ **2.0倍**、投入 **1.5倍** 遅い |
| OFFSET を使わない | 4,000件目で **2.19ms vs 0.36ms**（キーセット法は深さに依らない） |
| N+1 | 500件で **40〜64倍** 遅い。スロークエリログには出ない |
| 実行計画は件数で変わる | 同じ SQL が 50行では filesort、5,000行では PRIMARY の range |
| 層の検査の代金 | **147ns**（往復の 0.06%）。SQL ごとに結果を覚えると **33倍** 速くなる |

### 排他制御（[`docs/locking.md`](docs/locking.md)）

| 主張 | 実測 |
| --- | --- |
| `GET_LOCK` をプールごしに使うと壊れる | `RELEASE_LOCK` が **0**（解放できない）。ロックを持った接続がプールに残り、**別の処理が他人のロックを解放できた** |
| マイグレーションに行ロック（`FOR UPDATE`）は使えない | **DDL の暗黙コミットで外れる**。`GET_LOCK` は DDL をまたいで保持される |
| ロック無しで同時にマイグレーション | **7/8 が `Table already exists` で失敗**（DB は壊れないが、コンテナが起動できない） |
| 一意キーで「先に記録」する代替 | 8並列でエラー 0。ただし**途中で落ちると永久に当たらない**マイグレーションになる |
| ロック名はサーバ全体で共通 | 別データベースからでも同じ名前は取れない → スキーマ名を付ける |
| 長時間の担当に `GET_LOCK` | 20テナントで**接続20本を占有**し、プールが枯渇 → 期限つきリースを使う |
| 接続が切れたら | **即座に解放される**（プロセスが落ちてもロックは残らない） |

## 他のプロジェクトで使う

1. **レビュー観点として** — `.claude/skills/` の2つのスキル（ワーカー / リポジトリ層）。
   grep で当たる項目と、設計として読む項目に分けてある
2. **持って行けるテスト** — `internal/worker/tracker_test.go` の
   `TestStateに時刻やポインタを入れていない` は reflect だけで書いてあり、
   どのプロジェクトにもそのまま移植できる（一番踏まれる罠を機械的に止める）
3. **設定の検査** — `store.PoolConfig.Validate()` は §5.2 のチェックを実行時に行う。
   起動時に呼べば、`MaxIdleConns` の設定漏れを本番前に見つけられる
4. **リポジトリ層ごと** — `internal/repo/` は他プロジェクトへ移植できる形にしてある。
   最小構成は `guard.go`（SQL の検査）＋ `scope.go`（テナント束縛）＋ `repotest/`（テストの道具）
