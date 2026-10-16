# EXP-64 サブスクリプションの接続数を抑えるキャッシュが、リポジトリ層のテナントガードを迂回する

| | |
| --- | --- |
| Experiment | EXP-64 / subscription-cache-guard-bypass |
| Starting SHA | `87bc59ae9d14` (作業ツリーに未コミットの変更あり) |
| Meter version | `expkit/2` |
| Hypothesis (frozen before result) | 1) 購読者ごとに repo.Tenant(t) で読むと、他テナントの行は 0 だが、問い合わせは購読者数×poll、同時接続はプール上限まで張り付き待ちが出る（＝キャッシュ化したくなる理由）。 2) バックグラウンドで Unscoped に全テナントを読み、robot_id だけをキーにした共有キャッシュにすると、問い合わせは poll 1本に減るが、ID が衝突するテナント間で値が入れ替わり、自テナントに無い ID で他テナントの行が返る。エラーはどこにも出ない（DB を通らないのでガードが一度も走らない）。痕跡は Unscoped の回数（CrossTenant）だけ。 3) テナントごとに repo.Tenant(t) で1本読み、(tenant, id) の2段キャッシュに載せ、購読者のテナントを ctx から取ると、問い合わせはテナント数×poll（購読者数に依らない）で、他テナントの行は 0 に戻る。 |
| Environment | go1.26.0 linux/amd64 cpu=4 gomaxprocs=4 mysql=8.0.46 sha=87bc59ae9d14+dirty |
| Started / Ended | 2026-10-16T09:11:41Z / 2026-10-16T09:11:42Z |

## Workload

- `polls` = 20
- `pool_max_open` = 20
- `probe_subscribers` = 10
- `subscribers` = 80
- `tenants` = tenantA=100台, tenantB=60台（robot_id 1..60 は両方にある）

## Failure injection

- `cache_loader_ctx` = context.Background()（バックグラウンド poller にテナントが無い）

## Results

### ① 購読者ごとに repo.Tenant(t)（修正前）: ガードは効くが接続を食う — OK

| 数えたもの | 値 |
| --- | --- |
| cross_tenant_leak | 0 |
| db_queries | 1600 |
| deliveries | 1400 |
| errors | 0 |
| leak_id_probe | 0 |
| leak_key_collision | 0 |
| not_found | 200 |
| peak_conns_in_use | 20 |
| pool_wait_count | 1198 |
| unscoped_calls | 0 |

| 測ったもの | 値 |
| --- | --- |
| elapsed_ms | 254.367 |

- 問い合わせ = 購読者 80 × poll 20。同時接続はプール上限に張り付き、待ちが出る

### ② Unscoped で全テナント → map[robotID]（★キャッシュ化の事故） — **事故あり**

| 数えたもの | 値 |
| --- | --- |
| cross_tenant_leak | 680 |
| db_queries | 20 |
| deliveries | 1600 |
| errors | 0 |
| leak_id_probe | 200 |
| leak_key_collision | 480 |
| not_found | 0 |
| peak_conns_in_use | 1 |
| pool_wait_count | 0 |
| unscoped_calls | 20 |

| 測ったもの | 値 |
| --- | --- |
| elapsed_ms | 25.031 |

- 問い合わせは poll 1本に減ったが、ID 衝突で値が入れ替わり、B に無い ID で A の行が返る。errors=0
- ctx にテナントが無くても値が返る: true

### ③ テナントごとに repo.Tenant(t) → tenantcache(tenant, id)・tenant は ctx から（修正後） — OK

| 数えたもの | 値 |
| --- | --- |
| cross_tenant_leak | 0 |
| db_queries | 40 |
| deliveries | 1400 |
| errors | 0 |
| leak_id_probe | 0 |
| leak_key_collision | 0 |
| not_found | 200 |
| peak_conns_in_use | 1 |
| pool_wait_count | 0 |
| unscoped_calls | 0 |

| 測ったもの | 値 |
| --- | --- |
| elapsed_ms | 57.653 |

- 問い合わせ = テナント 2 × poll 20（購読者数に依らない）。B に無い ID は not_found
- ctx にテナントが無いと引けない: subguard: ctx にテナントが無い

## Verdict

接続数を抑えるために入れたサブスクリプションの共有キャッシュは、読み出しが DB を通らないため repo.Scope の :tenant 束縛を迂回する。バックグラウンド poller にテナントが無いことから Unscoped で全テナントを読み、robot_id だけをキーにすると、ID 衝突で値が入れ替わり・他テナントの ID で行が返る（エラーは 0）。テナントごとに repo.Tenant(t) で1本読み、(tenant, id) の2段キャッシュに載せ、購読者のテナントを ctx から取れば、問い合わせはテナント数×poll のまま漏れは 0。『ガードはキャッシュの境界にも置く』『Unscoped を業務経路で使ったら設計を疑う（CrossTenant を監視）』。

## 適用範囲

- MySQL 8.0 / 2 テナント（100 台・60 台、robot_id はテナント内連番）/ 購読者 80（うち ID 総当たり 10）/ poll 20 回
- プールは既定（MaxOpenConns=20）。購読者は poll ごとに同時に読む
- 漏れ = 購読者のテナント ≠ 渡した行の tenant_id。DB の行に持ち主を持たせて数えた

## 保証しない範囲・未検証

- キャッシュが Redis 等のプロセス外でも同型（キーにテナントが無い・読み出しが repo を通らない）だが、本実験はメモリ map で測った
- どちらのテナントの値が勝つかは読み込み順（ORDER BY tenant_id）で決まる。順序が変わると漏れる側が入れ替わるだけで件数の性質は同じ
- 無効化イベント（pub/sub）にテナントが無い場合の誤配信は本実験外（EXP-43 の経路で同型の事故になる）
- テナント数が非常に多い場合のテナントごと poll の本数は EXP-38/52（hub 共有・版で差分）で畳む前提

## 再利用できる成果物

- internal/subguardlab: PerSubscriber / FlatCache / TenantHub（3方式を同じ購読者で比べる）
- docs/subscription-cache-guard.md: バグシナリオと再発防止

