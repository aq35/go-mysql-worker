package subguardlab_test

// EXP-64: サブスクリプション化 → 接続を抑えるためにキャッシュ化 → リポジトリ層のガードが効かなくなった。
//
//	MYSQL_DSN=... go test ./internal/subguardlab/ -run TestEXP64 -v
//
// 同じ購読者（tenantA 40人・tenantB 40人、うち B の 10人は B に無い robot_id を指定）・同じ poll 回数で、
//
//	① 購読者ごとに repo.Tenant(t) で読む（修正前・ガードは効くが接続を食う）
//	② 全テナントを Unscoped で読んで map[robotID] に載せる（★事故・接続は減る）
//	③ テナントごとに repo.Tenant(t) で読んで tenantcache に載せ、購読者のテナントは ctx から（修正後）
//
// を比べ、DB 問い合わせ数・同時接続のピーク・他テナントの行を渡した回数を数える。

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"testing"

	"github.com/aq35/sample_manual/internal/expkit"
	"github.com/aq35/sample_manual/internal/model"
	"github.com/aq35/sample_manual/internal/mysqltest"
	"github.com/aq35/sample_manual/internal/repo"
	"github.com/aq35/sample_manual/internal/store"
	"github.com/aq35/sample_manual/internal/subguardlab"
)

const (
	tA = model.TenantID("tenantA")
	tB = model.TenantID("tenantB")

	robotsA = 100
	robotsB = 60
	rounds  = 20
)

func subscribers() []subguardlab.Subscriber {
	var subs []subguardlab.Subscriber
	for i := 0; i < 40; i++ { // A: 1..100 に散らす（1..60 は B と ID が衝突する）
		subs = append(subs, subguardlab.Subscriber{Tenant: tA, RobotID: int64(1 + i*robotsA/40), Owned: true})
	}
	for i := 0; i < 30; i++ { // B: 自分の robot（1..59）
		subs = append(subs, subguardlab.Subscriber{Tenant: tB, RobotID: int64(1 + i*2), Owned: true})
	}
	for i := 0; i < 10; i++ { // B: B に無い ID（61..97）。A にはある＝総当たり・取り違え
		subs = append(subs, subguardlab.Subscriber{Tenant: tB, RobotID: int64(robotsB + 1 + i*4), Owned: false})
	}
	return subs
}

func openRepo(t *testing.T, dsn string) *repo.DB {
	t.Helper()
	db, err := repo.Open(dsn, repo.Options{Logger: slog.New(slog.NewTextHandler(io.Discard, nil))})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = db.Close() })
	return db
}

func TestEXP64_サブスクリプションのキャッシュ化でガードが外れる(t *testing.T) {
	mysqltest.Serialize(t)
	ctx := context.Background()
	dsn := mysqltest.DSN(t)
	raw := mysqltest.Raw(t)
	if err := raw.PingContext(ctx); err != nil {
		t.Skipf("MySQL に繋がらない: %v", err)
	}
	if err := subguardlab.Setup(ctx, raw); err != nil {
		t.Fatal(err)
	}
	if err := subguardlab.Seed(ctx, raw, tA, robotsA); err != nil {
		t.Fatal(err)
	}
	if err := subguardlab.Seed(ctx, raw, tB, robotsB); err != nil {
		t.Fatal(err)
	}

	rec := expkit.NewRecorder("EXP-64", "subscription-cache-guard-bypass",
		"サブスクリプションの接続数を抑えるキャッシュが、リポジトリ層のテナントガードを迂回する")
	rec.Env(expkit.CaptureEnv(ctx, raw))
	rec.Freeze(
		"1) 購読者ごとに repo.Tenant(t) で読むと、他テナントの行は 0 だが、問い合わせは購読者数×poll、" +
			"同時接続はプール上限まで張り付き待ちが出る（＝キャッシュ化したくなる理由）。 " +
			"2) バックグラウンドで Unscoped に全テナントを読み、robot_id だけをキーにした共有キャッシュにすると、" +
			"問い合わせは poll 1本に減るが、ID が衝突するテナント間で値が入れ替わり、自テナントに無い ID で他テナントの行が返る。" +
			"エラーはどこにも出ない（DB を通らないのでガードが一度も走らない）。痕跡は Unscoped の回数（CrossTenant）だけ。 " +
			"3) テナントごとに repo.Tenant(t) で1本読み、(tenant, id) の2段キャッシュに載せ、購読者のテナントを ctx から取ると、" +
			"問い合わせはテナント数×poll（購読者数に依らない）で、他テナントの行は 0 に戻る。")

	subs := subscribers()
	probes := 0
	for _, s := range subs {
		if !s.Owned {
			probes++
		}
	}
	rec.Workload("tenants", fmt.Sprintf("%s=%d台, %s=%d台（robot_id 1..%d は両方にある）", tA, robotsA, tB, robotsB, robotsB))
	rec.Workload("subscribers", len(subs))
	rec.Workload("probe_subscribers", probes)
	rec.Workload("polls", rounds)
	rec.Workload("pool_max_open", store.DefaultPool().MaxOpenConns)
	rec.Injection("cache_loader_ctx", "context.Background()（バックグラウンド poller にテナントが無い）")

	type outcome struct {
		res   subguardlab.Result
		stats repo.Stats
	}
	run := func(db *repo.DB, src subguardlab.Source) outcome {
		r := subguardlab.Run(ctx, src, db.SQL(), subs, rounds)
		st := db.Stats()
		t.Logf("%-15s queries=%4d unscoped=%2d peak_inuse=%2d wait=%4d deliveries=%4d leak=%3d (collision=%d probe=%d) notfound=%d err=%d %v",
			src.Name(), st.Queries, st.CrossTenant, r.PeakInUse, r.WaitCount,
			r.Deliveries, r.CrossLeak, r.CrossCollision, r.CrossProbe, r.NotFound, r.Errors, r.Elapsed)
		return outcome{res: r, stats: st}
	}

	perDB := openRepo(t, dsn)
	per := run(perDB, &subguardlab.PerSubscriber{DB: perDB})
	flatDB := openRepo(t, dsn)
	flatSrc := &subguardlab.FlatCache{DB: flatDB}
	flat := run(flatDB, flatSrc)
	hubDB := openRepo(t, dsn)
	hub := &subguardlab.TenantHub{DB: hubDB}
	hubOut := run(hubDB, hub)

	// 修正後の境界: テナントの無い ctx（バックグラウンド・認証漏れ）では引けない
	_, _, noTenantErr := hub.Lookup(ctx, 1)
	// 事故側は、テナントの無い ctx でも普通に値が返る（境界が無い）
	_, flatNoTenantOK, _ := flatSrc.Lookup(ctx, 1)

	variant := func(name string, o outcome, accident bool, notes ...string) expkit.Variant {
		return expkit.Variant{
			Name:     name,
			Accident: accident,
			Counters: map[string]int64{
				"db_queries":         o.stats.Queries,
				"unscoped_calls":     o.stats.CrossTenant,
				"peak_conns_in_use":  int64(o.res.PeakInUse),
				"pool_wait_count":    o.res.WaitCount,
				"deliveries":         o.res.Deliveries,
				"cross_tenant_leak":  o.res.CrossLeak,
				"leak_key_collision": o.res.CrossCollision,
				"leak_id_probe":      o.res.CrossProbe,
				"not_found":          o.res.NotFound,
				"errors":             o.res.Errors,
			},
			Metrics: map[string]float64{"elapsed_ms": float64(o.res.Elapsed.Microseconds()) / 1000},
			Notes:   notes,
		}
	}
	rec.Add(variant("① 購読者ごとに repo.Tenant(t)（修正前）: ガードは効くが接続を食う", per, false,
		"問い合わせ = 購読者 80 × poll 20。同時接続はプール上限に張り付き、待ちが出る"))
	rec.Add(variant("② Unscoped で全テナント → map[robotID]（★キャッシュ化の事故）", flat, true,
		"問い合わせは poll 1本に減ったが、ID 衝突で値が入れ替わり、B に無い ID で A の行が返る。errors=0",
		fmt.Sprintf("ctx にテナントが無くても値が返る: %v", flatNoTenantOK)))
	rec.Add(variant("③ テナントごとに repo.Tenant(t) → tenantcache(tenant, id)・tenant は ctx から（修正後）", hubOut, false,
		"問い合わせ = テナント 2 × poll 20（購読者数に依らない）。B に無い ID は not_found",
		fmt.Sprintf("ctx にテナントが無いと引けない: %v", noTenantErr)))

	// ---- 検証 ----
	nSubs := int64(len(subs))
	if per.res.CrossLeak != 0 || per.res.Errors != 0 {
		t.Errorf("①で越境/エラー: leak=%d err=%d", per.res.CrossLeak, per.res.Errors)
	}
	if per.stats.Queries != nSubs*rounds {
		t.Errorf("①の問い合わせが 購読者×poll でない: %d", per.stats.Queries)
	}
	if per.res.NotFound != int64(probes*rounds) {
		t.Errorf("①で B に無い ID が not_found にならない: %d", per.res.NotFound)
	}

	if flat.res.CrossCollision == 0 || flat.res.CrossProbe == 0 {
		t.Errorf("②で事故が再現しない（防止策を外しても落ちない実験になっている）: collision=%d probe=%d",
			flat.res.CrossCollision, flat.res.CrossProbe)
	}
	if flat.res.Errors != 0 {
		t.Errorf("②はエラー無しで漏れるはず: err=%d", flat.res.Errors)
	}
	if flat.stats.Queries != rounds || flat.stats.CrossTenant != rounds {
		t.Errorf("②の問い合わせ/Unscoped が poll 回数でない: q=%d unscoped=%d", flat.stats.Queries, flat.stats.CrossTenant)
	}
	if !flatNoTenantOK {
		t.Error("②はテナント無しの ctx でも値を返すはず")
	}

	if hubOut.res.CrossLeak != 0 || hubOut.res.Errors != 0 {
		t.Errorf("③で越境/エラー: leak=%d err=%d", hubOut.res.CrossLeak, hubOut.res.Errors)
	}
	if hubOut.stats.Queries != 2*rounds || hubOut.stats.CrossTenant != 0 {
		t.Errorf("③の問い合わせが テナント数×poll でない / Unscoped を使った: q=%d unscoped=%d",
			hubOut.stats.Queries, hubOut.stats.CrossTenant)
	}
	if hubOut.res.NotFound != int64(probes*rounds) {
		t.Errorf("③で B に無い ID が not_found にならない: %d", hubOut.res.NotFound)
	}
	if hubOut.res.Deliveries != per.res.Deliveries {
		t.Errorf("③と①で配った件数が違う（正しい値を全部配れていない）: %d vs %d", hubOut.res.Deliveries, per.res.Deliveries)
	}
	if !errors.Is(noTenantErr, subguardlab.ErrNoTenant) {
		t.Errorf("③はテナント無しの ctx を拒むはず: %v", noTenantErr)
	}
	if hubOut.res.PeakInUse >= per.res.PeakInUse {
		t.Errorf("③で同時接続が減っていない: hub=%d per=%d", hubOut.res.PeakInUse, per.res.PeakInUse)
	}

	rec.Scope(
		"MySQL 8.0 / 2 テナント（100 台・60 台、robot_id はテナント内連番）/ 購読者 80（うち ID 総当たり 10）/ poll 20 回",
		"プールは既定（MaxOpenConns=20）。購読者は poll ごとに同時に読む",
		"漏れ = 購読者のテナント ≠ 渡した行の tenant_id。DB の行に持ち主を持たせて数えた",
	)
	rec.Uncertain(
		"キャッシュが Redis 等のプロセス外でも同型（キーにテナントが無い・読み出しが repo を通らない）だが、本実験はメモリ map で測った",
		"どちらのテナントの値が勝つかは読み込み順（ORDER BY tenant_id）で決まる。順序が変わると漏れる側が入れ替わるだけで件数の性質は同じ",
		"無効化イベント（pub/sub）にテナントが無い場合の誤配信は本実験外（EXP-43 の経路で同型の事故になる）",
		"テナント数が非常に多い場合のテナントごと poll の本数は EXP-38/52（hub 共有・版で差分）で畳む前提",
	)
	rec.Artifact(
		"internal/subguardlab: PerSubscriber / FlatCache / TenantHub（3方式を同じ購読者で比べる）",
		"docs/subscription-cache-guard.md: バグシナリオと再発防止",
	)

	files, err := rec.Save(
		"接続数を抑えるために入れたサブスクリプションの共有キャッシュは、読み出しが DB を通らないため repo.Scope の :tenant 束縛を" +
			"迂回する。バックグラウンド poller にテナントが無いことから Unscoped で全テナントを読み、robot_id だけをキーにすると、" +
			"ID 衝突で値が入れ替わり・他テナントの ID で行が返る（エラーは 0）。テナントごとに repo.Tenant(t) で1本読み、" +
			"(tenant, id) の2段キャッシュに載せ、購読者のテナントを ctx から取れば、問い合わせはテナント数×poll のまま漏れは 0。" +
			"『ガードはキャッシュの境界にも置く』『Unscoped を業務経路で使ったら設計を疑う（CrossTenant を監視）』。")
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("結果: %v", files)
}
