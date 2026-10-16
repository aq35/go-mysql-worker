# スケジュール・命令・実績の DB 表現とワーカーの頻度（EXP-12）

- 実験: `internal/cadencelab` / `MYSQL_DSN=... go test ./internal/cadencelab/ -run TestEXP12 -v`
- スキーマ: `internal/cadencelab/schema.sql`
- 結果: `docs/results/exp-12/exp-12-worker-cadence.md`

## DB でどう表すか（3つの表）

テナント先頭の複合主キー（他の表と同じ規約）で、テナント局所に読めるようにする。

### `cmd_schedule` — 予定（命令を生む）
`once`（一度）か `interval`（周期）。`next_run_at` が来たら命令を1件生む。
索引 `(tenant_id, enabled, next_run_at)` で「そろそろ動かす予定」を引く。

### `cmd_command` — 命令（ワーカーが捌くキュー / outbox 兼用）
- `state ENUM('pending','dispatched','acked','done','failed','unknown')`。
  `unknown` は EXP-1 の **OUTCOME_UNKNOWN**（出したが結果不明）。
- `idem_key`（テナント内 UNIQUE）で**二重発行**を防ぐ。
- `fence`（lease の番号）で**古い担当の発行を弾く**（EXP-2）。
- `scheduled_for` で due 判定。
- ★ホットパスの索引 `poll (tenant_id, state, scheduled_for)`。
  ワーカーの問い合わせ `WHERE tenant_id=? AND state='pending' AND scheduled_for<=? ORDER BY scheduled_for` が
  テナント局所の range scan になる。

### `cmd_result` — 実績（独立に観測して残す）
`action` の戻り値を receipt にしない（EXP-1 の禁止事項）。**独立に観測**した結果を追記する。
1命令に複数回の観測（出した直後 `unknown` → 後で `success`）がありうるので
`PRIMARY KEY (tenant_id, command_id, observed_at)` の追記型。

実績が予定ではなく**命令**を参照する理由（予定は UPDATE される・周期予定は1行から何回も走る）、名前のスナップショットを
持たせるか・いつ入れるかは [plan-actual-reference](plan-actual-reference.md)。

## ワーカーはどれくらいの頻度で引くか（実測）

命令 600 件を 3 秒に散らし（到着 200/s）、ポーリング間隔を振った。
**dispatch 遅延 = `scheduled_for` から実際に掴むまで。**

| interval | dispatch p95 | 問い合わせ回数 |
| --- | --- | --- |
| 100ms | ~190ms | 17 |
| 250ms | ~350ms | 10 |
| 500ms | ~700ms | 6 |
| 1s | ~1.4s | 4 |

→ **十分な batch で毎回 due を捌けるなら、遅延 p95 ≈ ポーリング間隔。**
だから間隔は「**許容する dispatch 遅延**」で決める。速くするほど遅延は下がるが問い合わせは増える。

### 先に満たす制約（間隔より重要）

**`batch / interval ≥ 到着レート`。** これを割ると、間隔と無関係に backlog が溜まり遅延が発散する。

| 条件 | 結果 |
| --- | --- |
| batch 50 / interval 500ms（処理能力 100/s）< 到着 200/s | p95 が **4.2 秒**まで発散、追いつかない |

だから間隔を詰める前に、**1回のポーリングで捌ける件数 × 頻度が到着レートを上回る**ことを確かめる。

### 到着が疎なら「速く引く」は無駄

命令 30 件を 3 秒に散らし（到着 10/s）、捌き終わった後もポーリングを続けた。

| interval | 空振りの問い合わせ | dispatch p95 |
| --- | --- | --- |
| 100ms | **19 回** | ~104ms |
| 250ms | 7 回 | ~250ms |
| 1s | 1 回 | ~940ms |

速く引くほど**空振り（0 件の問い合わせ）が増えるだけ**。遅延は間隔で決まる。

### wake（signal 型）— 疎な到着での最適解

producer が「due が来た」と in-process で起こす。ポーリングと違い空振りしない。

| 方式（到着 10/s） | 問い合わせ | 空振り | dispatch p95 |
| --- | --- | --- | --- |
| 100ms ポーリング | 48 | 19 | 104ms |
| **wake（signal 型）** | 30 | **0** | **6ms** |

**空振りゼロで、遅延は桁違いに小さい**（104ms → 6ms）。

★ただし「掴めた直後に間を置かず続ける」busy-continue は wake ではない。
連続到着（200/s）では結局ずっと引くことになり、ポーリングより問い合わせが増える（実験で反証）。
本物の wake は**イベント signal**で起こす。
また MySQL に `LISTEN/NOTIFY` は無いので、これは**同一プロセス内の producer→worker**の話。
別プロセス跨ぎで押し込むなら、別の仕組み（メッセージキュー等）が要る。

## 決め方（まとめ）

1. **処理能力を先に確保**: `batch/interval ≥ 到着レート`。
2. **間隔 = 許容 dispatch 遅延**（p95 ≈ 間隔）。
3. **到着が疎**なら、速いポーリングは空振りの無駄。間隔を伸ばすか **signal 型 wake**。
4. **複数テナント**は jitter で山をずらす（同時刻に全テナントが一斉に引かない）。

## テナントワーカーの制約

1テナント1ワーカーを前提に、以下を守る（無いと「普段は動く」まま壊れる）。

| 制約 | なぜ / どう |
| --- | --- |
| **1テナント1担当** | lease + fence（EXP-2）。二重起動で二重発行・接続二重を防ぐ。命令の `fence` と照合し、古い担当の発行を弾く |
| **冪等** | `idem_key` で二重発行を防ぐ。dispatch 後に落ちても、再発行が重複を生まない |
| **OUTCOME_UNKNOWN を潰さない** | 出した後に結果不明なら `unknown` で残し、`cmd_result` で独立観測してから確定（自動再実行しない） |
| **接続予算** | テナント数 × ワーカーのプール ≤ DB 予算（`internal/poolbudget`、EXP-5） |
| **公平性** | 1テナントが DB/ロボットを占有しないよう per-tenant quota（EXP-4）。バッチ上限で1周の仕事を区切る |
| **graceful shutdown** | 進行中の命令を中途半端にしない。lease を返してから落ちる（EXP-3）。goroutine を残さない |
| **時刻** | due 判定は DB 時刻か fence で。コンテナのローカル時計を信じない（EXP-2 の時計ずれ） |
| **メモリのテナント分離** | キャッシュは `internal/tenantcache`（(tenant,key) の2段）。lease を失ったら `EvictTenant` で捨てる（古い値を配らない） |
| **状態はメモリに"だけ"置かない** | 受けた命令・結果は DB に落とすまでが仕事（EXP-1 の outbox）。メモリはキャッシュで、消えても DB から再構築できる形に |
