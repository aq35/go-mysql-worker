# Go アプリ 作る・レビュー チェックリスト（全網羅）

Go + MySQL の常時稼働アプリ（Worker / Web API）を**作るとき・レビューするとき**に上から確認する
リスト。各行は **やること / なぜ / どう確認するか / 根拠(記事)**。用語は [用語集](glossary.md)、
数字は [早見表](reference-numbers.md)、全体像は [design-principles](design-principles.md)。

- **作るとき**: A→K の順に設計判断していく（上ほど後から変えにくい）。
- **レビューするとき**: 各行の「どう確認」を実行し、✅/⚠️/❌ を付ける。⚠️❌ は根拠記事へ。

> 機械チェックは1コマンドで回る: `go build ./... && go vet ./... && go run ./cmd/sqllint ./...`

---

## A. 設計を始める前（後から変えにくい順）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| マルチテナントの分離方針を決める | 全設計の土台。後から直すのは激痛 | テナントは何で識別？どこで強制？ | [security-layers](security-layers.md) |
| Worker をテナント単位か共有かを決める | 分離・コスト・公平性のトレードオフ | → [worker-tenancy](worker-tenancy.md) | [worker-tenancy](worker-tenancy.md) |
| 容量の当たりを付ける（1vCPU/2GB で何本/何件） | 律速が種類で違う（SSE=メモリ, Worker=DB, Web=CPU） | [worked-examples](worked-examples.md) の計算に自分の数字を代入 | [capacity](capacity.md) |
| Web と Worker を別プロセスに分ける | 埋まる資源が逆。相乗りは互いの足を引く | デプロイ単位が分かれているか | [web-worker-split](web-worker-split.md) |

## B. スキーマ・DBアクセス層（repository）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| 主キーは連番 BIGINT（UUID なら BINARY(16)＋時刻順） | ランダム PK は挿入が遅く index 肥大 | PK 型を確認。CHAR(36) UUID は❌ | [primary-key](primary-key.md) |
| DB アクセスは repository 層に閉じる（生 `*sql.DB` を層外で触らない） | 規約（テナント強制・LIMIT）を1箇所に集約 | `sqllint` の rawdb/layerimport | [repository-layer](repository-layer.md) / [static-analysis](static-analysis.md) |
| SELECT に LIMIT を必須化 | 暴走スキャンを止める | `sqllint`（LIMIT 無し SELECT を検出） | [repository-layer](repository-layer.md) |
| 太いメモ列は別表 or off-page（一覧の舐めから外す） | inline の太い列は全体走査が10倍重い | 一覧クエリが太い列を読んでいないか | [table-split](table-split.md) / [column-projection](column-projection.md) |
| charset は列ごとに選ぶ（ASCII 列に utf8mb4 を乱用しない） | utf8mb4 は4B/字で index 長 3072B に当たる | 長い索引列のバイト長。長いなら prefix 索引 | [charset](charset.md) |
| マイグレーションは expand/contract（前方後方互換） | 途中クラッシュ・無停止デプロイに耐える | 途中状態でも旧新コードが動くか | [zero-downtime-migration](zero-downtime-migration.md) / [migration-crash](migration-crash.md) |

## C. クエリ・性能

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| 重さは「走査行数 × 行幅 × 取る列」で考える | 総行数でなく走査行数で決まる | EXPLAIN の rows。covering index か | [date-search](date-search.md) |
| ページングは keyset（OFFSET を使わない） | OFFSET は後ろほど遅い | `WHERE id > ?` になっているか | [date-search](date-search.md) |
| N+1 を DataLoader 等でまとめる | 1件ずつ引くと往復が爆発 | 一覧＋関連の往復数。`sqllint` loopquery | [graphql](graphql.md) / [fanout](fanout.md) |
| `SELECT *` を避け必要列だけ射影 | 太い列を1行ずつ取りに行くと最悪 | クエリの列指定。off-page 列を取っていないか | [column-projection](column-projection.md) |
| 重いクエリは実行前にコスト見積りで弾く | スロークエリログに出る頃は手遅れ | GuardedQuery / ErrTooCostly を通すか | [query-cost-gate](query-cost-gate.md) |
| bulk 書き込みは multi-row＋1トランザクション | 単発×N は往復＋コミットで桁遅い（実測70x） | INSERT がループ単発になっていないか | [bulk-insert](bulk-insert.md) |

## D. 書き込みの正しさ（トランザクション・冪等・外部作用）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| 分離レベルは既定 RR のまま（変える時は根拠を持つ） | RR は再読安定＋gap ロックで phantom 防止 | 明示的に RC にしていないか。理由は？ | [isolation](isolation.md) |
| UPDATE/DELETE は影響行数を宣言して確かめる | WHERE ミス・更新消失の検出 | `sqllint` rowsaffected | [repository-layer](repository-layer.md) |
| mutation は冪等キーで二度目を弾く | 再送・二重実行に耐える | uq_idem 等の一意制約があるか | [graphql](graphql.md) |
| 外部作用は transactional outbox で | DB コミットと送信予約を1トランザクションに | 送信が別トランザクションになっていないか | [outbox](outbox.md) |
| 外部作用の後のクラッシュを `OUTCOME_UNKNOWN` 扱い | 「やった/記録した」のズレを後で照合 | 楽観的に「成功」と記録していないか | [crash-effects](crash-effects.md) |
| エラーは分類してリトライ（恒久は fail-fast） | 何でも retry は DB を無駄に叩く | 1062 等をリトライしていないか | [retry](retry.md) |
| 失敗し続ける命令は dead-letter に隔離 | 無限リトライでキューを詰まらせない | 試行上限があるか | [dead-letter](dead-letter.md) |

## E. Worker（常駐処理）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| ポーリングはテナント単位に coalesce（1台ずつ引かない） | per-row は 40,000 クエリ/秒で DB 飽和 | 1台ずつ join していないか | [fanout](fanout.md) / [why-necessary](why-necessary.md) |
| 1回のポーリングで batch 件まとめる | 処理量 ≒ batch / interval | ポーリング頻度と batch サイズ | [scheduling](scheduling.md) |
| 二重稼働を lease＋fence で防ぐ | 死んだと思ったワーカーが生きている | 同一テナントを2ワーカーが処理しないか | [fencing](fencing.md) |
| 担当テナント数（IN サイズ）に上限 | IN 肥大で実行計画が崩れる | 担当割当の上限があるか | [owned-tenant-limit](owned-tenant-limit.md) |
| バックオフは指数＋ジッタ＋上限 | 一斉再試行の雪崩防止 | 固定間隔・無限リトライになっていないか | [adaptive-backoff](adaptive-backoff.md) |
| Worker プールは小さく、合計接続を予算内に | 接続予算は DB の天井 | poolbudget で合計を確認 | [redundancy](redundancy.md) / [pool-saturation](pool-saturation.md) |

## F. Web / API（Query・Mutation・Subscription）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| SSE/購読は共有 hub から配る（DB 接続を1:1で持たせない） | 接続数ぶん DB が枯れる | SSE 1本 = DB 接続 1本になっていないか | [sse-fan-in](sse-fan-in.md) |
| hub はテナント単位（混線ゼロを検証） | 混線は情報漏洩 | cross-leak を検証したか | [sse-fan-in](sse-fan-in.md) |
| hub のキャッシュ破棄は版で差分（丸ごと再読しない） | 台数比例の無駄（実測400x） | poll が全件を舐めていないか | [subscription-design](subscription-design.md) |
| 状態×タスクは1購読・型つき封筒・種類ごとに版 | 接続増やさず、順序も守る | 別 version か。高頻度は間引くか | [subscription-design](subscription-design.md) |
| 複雑度上限・深さ制限・レート制限 | 重いクエリで殺されない | 上限が設定されているか | [graphql](graphql.md) |
| 任意クエリを受けない（永続化クエリ allowlist） | 受付制御 | allowlist があるか | [graphql](graphql.md) |

## G. セキュリティ（今回とくに重視）

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| 全クエリがテナント境界を越えない（`repo.Scope` で強制） | 越境は即・情報漏洩（境界1本外すと全越境・[EXP-58](tenant-scope.md)） | テナントは ctx から？入力から取っていないか | [tenant-scope](tenant-scope.md) / [security-layers](security-layers.md) |
| キャッシュ／サブスクリプションの経路もテナント境界を越えない | 読み出しが DB を通らないので `repo.Scope` のガードが走らない。Unscoped＋`map[id]` で配信の 43% が他テナント・エラー0（[EXP-64](subscription-cache-guard.md)） | キーが `(tenant, id)` か。poller が Unscoped を使っていないか。`Get` が ctx のテナントを取るか | [subscription-cache-guard](subscription-cache-guard.md) |
| テナントは ctx から取得（クライアント入力を信じない） | なりすまし防止 | ハンドラが tenant を引数で受けていないか | [security-layers](security-layers.md) |
| フィールド単位の認可（@auth） | 見せてよい列はロールで違う | 制限列が誰でも見えていないか | [security-layers](security-layers.md) |
| 行レベル認可（この対象を操作してよいか） | 自テナント内でも権限は分かれる | canOperate/grant を通すか | [security-layers](security-layers.md) |
| DB 資格情報は最小権限＋定期ローテーション | 漏れても被害を限定・失効させる | 権限が広すぎないか。期限つきか | [credential-rotation](credential-rotation.md) / [secrets](secrets.md) |
| 秘密はテナント分離・有効期限つき | 秘密の平文・無期限は危険 | 秘密の格納方法 | [secrets](secrets.md) |
| 長寿命接続は定期 re-auth＋最大寿命 | 失権後も配信し続ける穴 | 接続時だけの認可になっていないか | [subscription-design](subscription-design.md) |
| エラーは秘匿・本番は内観 off | 内部エラー/スキーマを漏らさない | ErrorPresenter・introspection 設定 | [graphql](graphql.md) |
| ブラスト半径を意識（越境時に何テナント被害？） | 分離設計の妥当性 | → [worker-tenancy](worker-tenancy.md) のセキュリティ節 | [worker-tenancy](worker-tenancy.md) |

## H. 回復性・障害

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| DB 切断・フェイルオーバに耐える（張り直し・retry） | 接続はいつか切れる | 切断検知と再接続があるか | [db-resilience](db-resilience.md) |
| 過負荷は backpressure で受付を絞る | 全部受けると全部倒れる | キュー上限・早期 429 があるか | [backpressure](backpressure.md) |
| graceful shutdown（途中の仕事を殺さない） | デプロイ・スケールインで欠損しない | 受付停止→in-flight 完了→close | [shutdown](shutdown.md) |
| context でクエリタイムアウト＋キャンセル伝播 | 遅いクエリが接続を占有 | 全クエリに ctx が通っているか | [query-timeout](query-timeout.md) |
| ノイジーネイバー対策（1テナントの暴走を隔離） | 1テナントが全体を巻き込む | 公平性の仕組みがあるか | [tenant-fairness](tenant-fairness.md) |

## I. 運用・観測

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| 履歴・冪等表は保持期間で刈る（パーティション DROP） | 放置すると無制限（実測 8.6GB/日規模） | 保持ジョブがあるか | [retention](retention.md) |
| メトリクスのラベルは値域を絞る（テナント id を直ラベルにしない） | 高カーディナリティで爆発 | ラベル設計 | [observability](observability.md) |
| バックアップ・復旧・破損検知を演習しておく | 壊れてからでは遅い | restore を試したか | [backup-restore](backup-restore.md) |
| 時刻は UTC 保存、境界（DST）は明示的に | ローカル時刻は飛ぶ/重なる | 保存が UTC か | [timezone](timezone.md) |

## J. コード品質・Go 固有

| やること | なぜ | どう確認 | 根拠 |
| --- | --- | --- | --- |
| メモリは「単価 × 件数」で予算内か | 「2GB あるから無限」ではない | 大きな slice/map の件数見積り | [reference-numbers](reference-numbers.md) |
| ホットパスの alloc を減らす（事前確保・Builder・boxing 回避） | allocs/op が GC 圧＝CPU を食う | ループ内の確保・`any` 詰め | [reference-numbers](reference-numbers.md) |
| 全 DB 呼び出しに context を渡す | キャンセル・タイムアウト伝播 | `sqllint` nocontext | [static-analysis](static-analysis.md) |
| ロジックを DB に固定しすぎない（移植性） | テスト・差し替えが利く | domain 契約に寄っているか | [sqlite](sqlite.md) |

## K. レビュー時の機械チェック（コマンド）

| チェック | コマンド | 何を見る |
| --- | --- | --- |
| ビルド | `go build ./...` | コンパイル |
| vet | `go vet ./...` | 明白なバグ |
| SQL 規約 lint | `go run ./cmd/sqllint ./...` | rawdb/loopquery/rowsaffected/nocontext/layerimport |
| バイナリ誤コミット | `bash scripts/check-no-binaries.sh` | 実行形式の混入 | 
| 依存整理 | `go mod tidy`（差分が出ないこと） | 不要依存 |
| 実験の再現 | `MYSQL_DSN=... EXP_RECORD=1 bash scripts/run-all.sh` | 主張が今も再現するか |

---

> このリストは [design-principles](design-principles.md)（考え方の一覧）を**行動に落としたもの**。
> 迷ったら各行の根拠記事に実測と具体例がある。**セキュリティ重視の設計判断**は
> [worker-tenancy](worker-tenancy.md) に集約。
