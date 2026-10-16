# 実験基盤（`internal/expkit`）

このリポジトリの実験は、**「良さそうな設計を書く」ためではなく「事故を再現して、
止まることを確かめる」ため**にある。そのための測定器がこのパッケージ。

## 実験が満たすこと

1. **修正前に事故を再現できる**（故障注入で、狙った地点で壊せる）
2. **防止策で事故が止まる**（同じ workload・同じ注入で before / after）
3. **防止策を外すとテストが落ちる**（`Variant.Accident` を持つ方式を必ず1つ残す）
4. **測定条件が保存される**（`Env`: Go / OS / CPU / MySQL 変数 / **実行 SHA**）
5. **適用範囲と未保証範囲が書かれる**（`Scope` / `Uncertainty`）
6. **他プロジェクトへ持ち込める**（`expkit` はこのリポジトリ固有の型に依存しない）

## 構成

| ファイル | 役割 |
| --- | --- |
| `env.go` | 測定条件の採取（Go・CPU・MySQL 変数・git SHA・作業ツリーの汚れ） |
| `latency.go` | 所要時間の記録と p50/p95/p99 |
| `sample.go` | goroutine 数・ヒープ・RSS・DB プールの時系列サンプリング |
| `recorder.go` | 結果の組み立てと保存（JSON + Markdown） |
| `killpoint.go` | **子プロセス側**の故障注入（名前つき地点で自分を SIGKILL / 一時停止） |
| `child.go` | **親（テスト）側**から子プロセスを起動・待ち合わせ・信号送出・終了判定 |

## 故障注入の考え方

外から時間を見計らって `kill` すると、実行ごとに落ちる場所が変わって再現しない。
そこで **プロセス自身が「この地点で死ぬ」と決める**。

```go
ks := expkit.NewKillSwitch()   // EXP_KILL_AT / EXP_PAUSE_AT を読む
...
ks.Point("after_external_effect")  // ここが指定されていれば自分に SIGKILL
```

- `EXP_KILL_AT=<地点>` … その地点で **SIGKILL**（defer も回復処理も走らない）
- `EXP_PAUSE_AT=<地点>` … その地点で止まり、親に印を出す（正確な位置へ SIGTERM を送るため）
- どの地点も通過時に `EXPPOINT <名前>` を標準出力へ出すので、**どこまで進んで死んだか**が残る

親側:

```go
bin := expkit.Build(t, "github.com/aq35/sample_manual/cmd/xxx")
c, _ := expkit.StartChild(ctx, bin, args, expkit.KillPointEnv+"=after_external_effect")
info, _ := c.Wait(30 * time.Second)   // info.Killed / info.Signal / info.Points
```

SIGKILL は捕捉できないので、自分で送っても外から送っても同じ。
一方 **graceful shutdown の実験（SIGTERM など）は捕捉されるので、
`EXP_PAUSE_AT` で正確な地点に止めてから親が送る**。

## 結果の保存

`docs/results/<unit>/<timestamp>-<name>.json` と `.md` の2つを書く。

- JSON は機械可読（後から比較・集計する）
- Markdown は報告フォーマット（Experiment / Starting SHA / Hypothesis / Environment /
  Workload / Failure injection / Results / Verdict / 適用範囲 / 未検証 / 再利用できる成果物）

**仮説は結果を見る前に固定する。** `Recorder.Freeze` を呼ぶ前に結果を足すと panic し、
`Freeze` を2回呼んでも panic する。数字が出てから仮説を書き換えることが、
構造的にできないようにしてある。

```go
r := expkit.NewRecorder("EXP-1", "outbox-crash", "外部effect途中のSIGKILL")
r.Env(expkit.CaptureEnv(ctx, db))
r.Freeze("naive retry は二重送信を出し、outbox + idempotency key は出さない")  // ★先に書く
r.Workload("requests", 200).Injection("kill_at", "after_external_effect")
r.Add(expkit.Variant{Name: "naive_retry", Accident: true, Counters: ...})
r.Add(expkit.Variant{Name: "outbox", Counters: ...})
r.Scope("MySQL 8.0 / 単一プロセス / 同一ホスト")
r.Uncertain("レプリカ構成・ネットワーク分断は未検証")
files, _ := r.Save("...")
```

## 実験テストの直列化（踏んだ罠）

`go test` は **パッケージを並列に実行する**（既定 `-p` = CPU 数）。
実験テストはグローバル設定（`innodb_flush_log_at_trx_commit`、`wait_timeout`）を変え、
同じ名前の実験用テーブルを作っては消すので、**並列に走ると互いを壊す**。

実際に踏んだ壊れ方:

- 別パッケージのテストが実験用テーブルを消し、`Table 'exp_uuid' doesn't exist` で落ちた
- 中断された実行が `wait_timeout=2` を残し、後続の実行が
  「接続が切られた（Error 4031）」で落ちた

**落ち方が実行のたびに変わるので、実装のバグに見えてしまう。** 対策は2つ。

1. `mysqltest.Serialize(t)` — MySQL を使う実験テストの先頭で呼ぶ。
   `GET_LOCK` で1つずつ実行する（接続を固定して取る。`docs/locking.md` 3.1）
2. `internal/mysqlfacts` の `TestMain` — 前回の残骸を戻してから始める
   （グローバル設定を変える実験は、開始時に既知の状態へ戻す）

## 測定器を疑う

`internal/expkit/expkit_test.go` は測定器そのものの検査。

- 分位点が既知の入力に対して正しいか
- 仮説の後出しができないか
- サンプラーが goroutine の山を捉えられるか
- 子プロセスが**指定地点で本当に SIGKILL され、その先へ進んでいないか**
- 指定地点で止めて信号を送れるか

**矛盾する数字が出たら、実装を説明する前にここを疑う。**

## 実験の単位

| Unit | 内容 | 状態 |
| --- | --- | --- |
| EXP-0 | 測定器の自己検査 | 済（`internal/expkit`） |
| EXP-1 | 外部 effect・crash・`OUTCOME_UNKNOWN` | 済（[docs/crash-effects.md](crash-effects.md)） |
| EXP-2 | lease / fencing / clock skew | 済（[docs/fencing.md](fencing.md)） |
| EXP-3 | graceful shutdown | 済（[docs/shutdown.md](shutdown.md)） |
| EXP-4 | backpressure・過負荷 | 済（[docs/backpressure.md](backpressure.md)） |
| EXP-5 | connection pool 飽和・RDS Proxy | 済（[docs/pool-saturation.md](pool-saturation.md)）／ Proxy は [rds-proxy.md](rds-proxy.md)（**LIVE_ENV_REQUIRED**） |
| EXP-6 | migration crash matrix | 済（[docs/migration-crash.md](migration-crash.md)） |
| EXP-7 | query plan・データ偏り | 済（[docs/query-plan-skew.md](query-plan-skew.md)） |
| EXP-8 | SQL guard の fuzz | 済（[docs/sql-guard-fuzz.md](sql-guard-fuzz.md)） |
| EXP-9 | `go/analysis` による保守性検査 | 済（[docs/static-analysis.md](static-analysis.md)） |
| EXP-10 | SQLite companion | 済（[docs/sqlite.md](sqlite.md)） |
| EXP-11 | backup / restore / corruption | 済（[docs/backup-restore.md](backup-restore.md)） |
| EXP-12 | ワーカーのポーリング頻度・スケジュール/命令/実績 | 済（[docs/scheduling.md](scheduling.md)） |
| EXP-13 | DB 資格情報のローテーション（graceful pool swap） | 済（[docs/credential-rotation.md](credential-rotation.md)） |
| EXP-14 | ポーリングの fan-out を畳む（テナント分離を保つ） | 済（[docs/fanout.md](fanout.md)） |
| EXP-15 | テーブル分割・パーティションと UPDATE 競合 | 済（[docs/table-split.md](table-split.md)） |
| EXP-16 | 担当テナント数（IN サイズ）の上限 | 済（[docs/owned-tenant-limit.md](owned-tenant-limit.md)） |
| EXP-17 | 適応的バックオフ | 済（[docs/adaptive-backoff.md](adaptive-backoff.md)） |
| EXP-18 | 日付範囲検索は何件で重くなるか | 済（[docs/date-search.md](date-search.md)） |
| EXP-19 | メモ列（失敗理由）を一覧行に同居させるか別表に分けるか | 済（[docs/column-split.md](column-split.md)） |
| EXP-20 | 予定/実績を primary とレプリカで読み分ける | 済（[docs/read-replica.md](read-replica.md)） |
| EXP-21 | 太い列の重さは inline/off-page で決まる・SELECT * の効き | 済（[docs/column-projection.md](column-projection.md)） |
| EXP-22 | ステータスでテーブルを分けるべきか | 済（[docs/status-table-split.md](status-table-split.md)） |
| EXP-23 | gqlgen のパフォーマンス（N+1・DataLoader・射影・ページ上限） | 済（[docs/graphql.md](graphql.md)） |
| EXP-24 | gqlgen のセキュリティ（テナント分離・複雑度 DoS・内観・エラー秘匿） | 済（[docs/graphql.md](graphql.md)） |
| EXP-25 | gqlgen の認可（@auth ディレクティブ・フィールド単位ロール） | 済（[docs/graphql.md](graphql.md)） |
| EXP-26 | gqlgen の受付制御（永続化クエリ allowlist・レート制限） | 済（[docs/graphql.md](graphql.md)） |
| EXP-27 | gqlgen mutation の冪等性・入力検証 | 済（[docs/graphql.md](graphql.md)） |
| EXP-28 | gqlgen の行レベル認可（この対象を操作してよいか） | 済（[docs/graphql.md](graphql.md)） |
| EXP-29 | Web と Worker の接続プール分離（共有 vs 分離） | 済（[docs/web-worker-split.md](web-worker-split.md)） |
| EXP-30 | 冗長化（複数レプリカ）でアプリはどうあるべきか | 済（[docs/redundancy.md](redundancy.md)） |
| EXP-31 | 1 vCPU/2GB タスクの容量（SSE 本数・Worker 処理量） | 済（[docs/capacity.md](capacity.md)） |
| EXP-32 | 無停止スキーマ変更（expand/contract） | 済（[docs/zero-downtime-migration.md](zero-downtime-migration.md)） |
| EXP-33 | DB 切断/フェイルオーバへの耐性 | 済（[docs/db-resilience.md](db-resilience.md)） |
| EXP-34 | ノイジーネイバー（テナント公平性） | 済（[docs/tenant-fairness.md](tenant-fairness.md)） |
| EXP-35 | タイムゾーン/DST のスケジューリング | 済（[docs/timezone.md](timezone.md)） |
| EXP-36 | クエリのタイムアウトとキャンセル伝播 | 済（[docs/query-timeout.md](query-timeout.md)） |
| EXP-37 | 可観測性（メトリクスのカーディナリティ・コスト） | 済（[docs/observability.md](observability.md)） |
| EXP-38 | 多数が同一テナントで SSE 購読するとき（DB ファンイン対策・hub 上限） | 済（[docs/sse-fan-in.md](sse-fan-in.md)） |
| EXP-39 | 動く SSE エンドポイント（poller＋fan-out＋レジストリ・1コンテナ） | 済（[docs/sse-fan-in.md](sse-fan-in.md)） |
| EXP-40 | SSE は何人まで（hub あり/なしの容量計算機） | 済（[docs/sse-fan-in.md](sse-fan-in.md)） |
| EXP-41 | gqlgen サブスクリプションを hub で作る | 済（[docs/graphql.md](graphql.md)） |
| EXP-42 | hub のテナント分離（混線ゼロ・poller 独立） | 済（[docs/sse-fan-in.md](sse-fan-in.md)） |
| EXP-43 | 複数プロセス跨ぎの SSE fan-out（pub/sub・topic=テナント分離） | 済（[docs/sse-fan-in.md](sse-fan-in.md)） |
| EXP-44 | トランザクショナル outbox（exactly-once の外部作用） | 済（[docs/outbox.md](outbox.md)） |
| EXP-45 | poison / dead-letter（無限リトライしない） | 済（[docs/dead-letter.md](dead-letter.md)） |
| EXP-46 | キャッシュ無効化（TTL / イベント失効 / stampede） | 済（[docs/cache.md](cache.md)） |
| EXP-47 | 順序・冪等消費（版で単調適用・逆順/重複に強い） | 済（[docs/event-ordering.md](event-ordering.md)） |
| EXP-48 | 保持期間の運用（日パーティションの DROP でロールフォワード） | 済（[docs/retention.md](retention.md)） |
| EXP-49 | 一時 vs 恒久エラーの分類とリトライ（fail-fast） | 済（[docs/retry.md](retry.md)） |
| EXP-50 | Go の変数・型ごとのメモリ単価（2GB に何件載るか） | 済（[docs/reference-numbers.md](reference-numbers.md)） |
| EXP-51 | ループとアロケーションの正規化コスト（ns/op・allocs/op） | 済（[docs/reference-numbers.md](reference-numbers.md)） |
| EXP-52 | hub のキャッシュ破棄の粒度（丸ごと vs 版で差分・400x） | 済（[docs/subscription-design.md](subscription-design.md)） |
| EXP-53 | 長寿命接続の途中失権（定期 re-authorization） | 済（[docs/subscription-design.md](subscription-design.md)） |
| EXP-54 | 主キー設計（連番 BIGINT vs ランダム UUID の肥大） | 済（[docs/primary-key.md](primary-key.md)） |
| EXP-55 | トランザクション分離レベル（RR vs RC・gap ロック） | 済（[docs/isolation.md](isolation.md)） |
| EXP-56 | bulk INSERT の正規化（単発/tx/prepared/multi-row） | 済（[docs/bulk-insert.md](bulk-insert.md)） |
| EXP-57 | utf8mb4 と index 長・照合（3072B 上限・_ci/_bin） | 済（[docs/charset.md](charset.md)） |
| EXP-58 | 共有ワーカーのテナントスコープ強制（越境の実測・cross-leak） | 済（[docs/tenant-scope.md](tenant-scope.md)） |
| EXP-59 | gqlgen subscription 1本あたりの実メモリ（容量見積りの裏取り） | 済（[docs/concern-subscription-capacity.md](concern-subscription-capacity.md)） |
| EXP-60 | 接続予算とオートスケール・ストーム（1040 拒否の実測・Guard） | 済（[docs/connection-budget.md](connection-budget.md)） |
| EXP-61 | 月予算での縦/横/Aurora の優先順位（律速→lever の判断モデル） | 済（[docs/cost-scaling-priority.md](cost-scaling-priority.md)） |
| EXP-62 | イベント駆動 worker（doorbell＋floor・完了は DB CAS・crash は reconcile） | 済（[docs/event-driven-worker.md](event-driven-worker.md)） |
| EXP-63 | テナント割り当てを DB lease で（均等10/10・失敗時 survivor が全20・二重所有0・fence 単調・静的ピンは orphan10） | 済（[docs/tenant-assignment.md](tenant-assignment.md)） |
| EXP-64 | サブスクリプションをキャッシュ化したらリポジトリ層のガードが外れた（Unscoped＋map[id] で配信の 43% が他テナント・エラー0／テナントごと Scope＋2段キーで漏れ0・問い合わせ40） | 済（[docs/subscription-cache-guard.md](subscription-cache-guard.md)） |

## 設計の早見表・原則

| 文書 | 内容 |
| --- | --- |
| [docs/reference-numbers.md](reference-numbers.md) | 正規化した数値の早見表（メモリ/ループ/DB往復/列幅/1vCPU2GB 理論値） |
| [docs/worked-examples.md](worked-examples.md) | Worker/Web 単体の実寸サンプル（メモリ・CPU 圧迫の計算＋mermaid 図） |
| [docs/why-necessary.md](why-necessary.md) | なぜ必要か（標準ワーカー/標準Web のメモリ・ストレージ計算 → 破綻 → 設計） |
| [docs/design-principles.md](design-principles.md) | 関心ごと × 考え方 × どうあるべき の一覧（漏れチェックつき） |
| [docs/db-ecs-complete.md](db-ecs-complete.md) | DB＋ECS で完結するモデル（SQS/EventBridge の向く/向かない・キャッシュの当て所・制約＋reconcile） |
| [docs/tenant-worker-capacity.md](tenant-worker-capacity.md) | per-tenant worker の容量見積り（20テナント/Aurora1000接続の現実値） |
| [docs/frontend-cache.md](frontend-cache.md) | フロントキャッシュは最後の一押し（真実は DB・不変/非鮮度/非認可だけ載せる） |
| [docs/sqs-eventbridge-limits.md](sqs-eventbridge-limits.md) | EventBridge/SQS の得意・苦手の深掘り（機構分解・10ユースケース・一生終わらない系＝常駐worker） |
| [docs/query-policy.md](query-policy.md) | クエリ方針（単純+バッチを既定・索引は狙って・巨大SQLはガード付き例外） |
| [docs/tenant-header-routing-vs-auth.md](tenant-header-routing-vs-auth.md) | テナントヘッダ：ルーティングOK/認可NG（越境防止の信頼境界） |
| [docs/worker-vs-web.md](worker-vs-web.md) | 関心ごと × Worker/Web 対応表（セキュリティ/lifecycle/性能/マイグレーション） |
| [docs/responsibilities.md](responsibilities.md) | Web と Worker に求められること（責務の入口・capstone） |
| ┗ [concern-security](concern-security.md) / [concern-lifecycle](concern-lifecycle.md) / [concern-performance](concern-performance.md) / [concern-migration](concern-migration.md) | 各関心の詳細記事（目的・前提・対応・意味と効果・図） |
| ┗ [concern-subscription-capacity](concern-subscription-capacity.md) | gqlgen subscription は何本提供できるか（律速・hub キャッシュ・資材の家計簿） |
| [docs/architecture.md](architecture.md) | システム構成図＋分かりにくい用語を小さな図で補強 |
| [docs/glossary.md](glossary.md) | 用語集（律速・冪等・fan-out 等を平易な説明＋英語＋たとえで） |
| [docs/deep-dives.md](deep-dives.md) | 図で深掘り（索引先頭tenant_id/ワーカーの単位/サブスク性能/GET_LOCK vs lease） |
| [docs/checklist.md](checklist.md) | 作る・レビューするときのやることリスト（全網羅・表・記事リンク付き） |
| [docs/worker-tenancy.md](worker-tenancy.md) | Worker をテナント単位か共有か（トレードオフ・セキュリティ重視の結論） |
| [docs/web-worker-deploy.md](web-worker-deploy.md) | Web/Worker 分離時のデプロイ（時期/スペック/オートスケール差・CodePipeline 2経路・skew と新旧共存・多重化ワーカー lease 引継・blue/green ミスマッチ・図つき） |
| [docs/worker-connection-model.md](worker-connection-model.md) | 常時接続ワーカー（WS多重化・再接続・1コンテナ1接続の是非・outbox） |
| [docs/domain-themes.md](domain-themes.md) | 適用テーマ候補と具体テーブル設計（マルチテナント越境禁止のDDL雛形） |
| [docs/subscription-design.md](subscription-design.md) | hub の破棄粒度・途中失権・状態×タスクの購読設計 |
| [docs/plan-actual-reference.md](plan-actual-reference.md) | 予定と実績の分離: ID 参照は命令へ・名前は実行時点のスナップショット（`*_at_run`）・後から足すなら NULL＝不明 |
| [docs/subscription-cache-guard.md](subscription-cache-guard.md) | バグシナリオ: サブスクリプションをキャッシュ化したらリポジトリ層のテナントガードが外れた（EXP-64） |

## 実行時ガード

| 文書 | 内容 |
| --- | --- |
| [docs/query-cost-gate.md](query-cost-gate.md) | 走査見込みで重いクエリを実行前に弾く（`repo.GuardedQuery` / `ErrTooCostly`） |

## 健全性・移植

| 文書 | 内容 |
| --- | --- |
| [docs/mysql-stop-forensics.md](mysql-stop-forensics.md) | MySQL 停止の原因調査（コンテナ再取得）と suite の完了条件（postflight） |
| [docs/binary-in-history.md](binary-in-history.md) | 誤コミットした binary の記録と再発防止 |
| [docs/kas-adoption.md](kas-adoption.md) | KAS への採用方針（各結果の分類） |
| `internal/kascontract` | EXP-10 の結果を domain 契約 + 言語非依存ベクタにしたもの |
| [docs/secrets.md](secrets.md) | 有効期限つき秘密（SecretManager 相当）の扱い・テナント分離・排他 |
| [docs/samber-io.md](samber-io.md) | samber/lo・mo の使いどころと層の線引き |

## セキュリティ設計

| 文書 | 内容 |
| --- | --- |
| [docs/security-layers.md](security-layers.md) | テナント分離の多層防御（Web / Worker の図）と、Web・Worker のセキュリティ指針 |
| [docs/graphql.md](graphql.md) | gqlgen のセキュリティ（テナント分離・認可・複雑度・allowlist・レート制限） |
