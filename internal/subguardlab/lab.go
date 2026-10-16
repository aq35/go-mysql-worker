// Package subguardlab は EXP-64（サブスクリプションをキャッシュ化したらリポジトリ層のガードが外れた）の実験本体。
//
// 起きる順番（バグシナリオ）:
//
//  1. 状態の配信をサブスクリプション（gqlgen / SSE）にした。購読者1人ごとに DB を引いていた。
//  2. 購読者が増えて接続プールが埋まった（EXP-5 / EXP-60）。接続数を抑えるため、
//     バックグラウンドで1回だけ読んで共有キャッシュに載せ、購読者はキャッシュから読むようにした。
//  3. バックグラウンドの読み込みには「リクエストの主体（テナント）」が無い。ctx にテナントが無いので
//     repo.Tenant(t) が使えず、repo.Unscoped（や生 SQL）で全テナントを1回で読んだ。
//     キャッシュのキーは robot_id だけ。購読者の読み出しは DB を通らないので、
//     repo.Scope の :tenant 束縛（＝リポジトリ層のガード）が一度も効かない。
//  4. 結果: robot_id が衝突したテナント同士で値が入れ替わり、存在しない ID を指定すると
//     他テナントの行が返る（情報漏洩）。どこもエラーにならない。
//
// 3方式を同じ購読者・同じ poll 回数で比べる:
//
//   - PerSubscriber: 購読者ごとに repo.Tenant(t) で読む（ガードは効く。接続を食う）
//   - FlatCache:     全テナントを Unscoped で読み、map[robotID] に載せる（★事故）
//   - TenantHub:     テナントごとに repo.Tenant(t) で1回読み、tenantcache（tenant 必須）に載せる。
//     購読者のテナントは ctx から取る（購読引数からは取らない）
package subguardlab

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"sync"

	"github.com/aq35/sample_manual/internal/model"
	"github.com/aq35/sample_manual/internal/repo"
	"github.com/aq35/sample_manual/internal/tenantcache"
)

// Setup は sub_robot 表を作る。(tenant_id, robot_id) が PK。robot_id はテナント内の連番なので、
// テナントを跨ぐと同じ robot_id が普通に存在する（＝キャッシュキーが衝突する前提）。
func Setup(ctx context.Context, db *sql.DB) error {
	for _, q := range []string{
		"DROP TABLE IF EXISTS sub_robot",
		`CREATE TABLE sub_robot (
		   tenant_id VARCHAR(32) NOT NULL,
		   robot_id  BIGINT      NOT NULL,
		   name      VARCHAR(64) NOT NULL,
		   ver       BIGINT      NOT NULL,
		   PRIMARY KEY (tenant_id, robot_id)
		 ) ENGINE=InnoDB`,
	} {
		if _, err := db.ExecContext(ctx, q); err != nil {
			return fmt.Errorf("subguard setup: %w", err)
		}
	}
	return nil
}

// Seed は tenant に robot_id=1..n を入れる（name はテナント名入り。漏れたら目で分かる）。
func Seed(ctx context.Context, db *sql.DB, tenant model.TenantID, n int) error {
	var b strings.Builder
	b.WriteString("INSERT INTO sub_robot (tenant_id, robot_id, name, ver) VALUES ")
	args := make([]any, 0, n*3)
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteString(",")
		}
		b.WriteString("(?,?,?,1)")
		args = append(args, string(tenant), i, fmt.Sprintf("%s-robot-%d", tenant, i))
	}
	_, err := db.ExecContext(ctx, b.String(), args...)
	return err
}

// Robot は購読者に配る1台ぶん。Tenant は「その行の持ち主」（漏れの判定に使う）。
type Robot struct {
	Tenant model.TenantID
	ID     int64
	Name   string
	Ver    int64
}

// ---- リクエストの主体（認証ミドルウェアが載せる。gql.WithTenant と同じ考え方）----

type tenantKey struct{}

// ErrNoTenant は ctx にテナントが無い（認証を通っていない／バックグラウンド goroutine）。
var ErrNoTenant = errors.New("subguard: ctx にテナントが無い")

// WithTenant は検証済みテナントを ctx に載せる。
func WithTenant(ctx context.Context, t model.TenantID) context.Context {
	return context.WithValue(ctx, tenantKey{}, t)
}

// TenantFrom は ctx のテナント。無ければエラー（黙って全テナントにしない）。
func TenantFrom(ctx context.Context) (model.TenantID, error) {
	t, ok := ctx.Value(tenantKey{}).(model.TenantID)
	if !ok || t == "" {
		return "", ErrNoTenant
	}
	return t, nil
}

// Source は購読者に値を渡す経路。Refresh は poll 1回ぶん（バックグラウンド・テナント無しの ctx で呼ぶ）、
// Lookup は購読者1人ぶん（購読者の ctx で呼ぶ）。
type Source interface {
	Name() string
	Refresh(ctx context.Context, tenants []model.TenantID) error
	Lookup(ctx context.Context, robotID int64) (Robot, bool, error)
}

const selectByID = `SELECT tenant_id, robot_id, name, ver FROM sub_robot
                    WHERE tenant_id = :tenant AND robot_id = ?`

const selectTenant = `SELECT tenant_id, robot_id, name, ver FROM sub_robot
                      WHERE tenant_id = :tenant ORDER BY robot_id LIMIT 1000`

func scanRobots(rows *sql.Rows) ([]Robot, error) {
	defer func() { _ = rows.Close() }()
	var out []Robot
	for rows.Next() {
		var r Robot
		var t string
		if err := rows.Scan(&t, &r.ID, &r.Name, &r.Ver); err != nil {
			return nil, err
		}
		r.Tenant = model.TenantID(t)
		out = append(out, r)
	}
	return out, rows.Err()
}

// ---- ① 修正前（ガードは効く・接続を食う）----

// PerSubscriber は購読者ごと・poll ごとに repo.Tenant(t) で1行読む。
type PerSubscriber struct{ DB *repo.DB }

func (s *PerSubscriber) Name() string { return "per-subscriber" }

func (s *PerSubscriber) Refresh(context.Context, []model.TenantID) error { return nil }

func (s *PerSubscriber) Lookup(ctx context.Context, robotID int64) (Robot, bool, error) {
	t, err := TenantFrom(ctx)
	if err != nil {
		return Robot{}, false, err
	}
	var r Robot
	var owner string
	err = s.DB.Tenant(t).QueryRow(ctx, "sub.robot", selectByID, robotID).Scan(&owner, &r.ID, &r.Name, &r.Ver)
	if errors.Is(err, repo.ErrNotFound) {
		return Robot{}, false, nil
	}
	if err != nil {
		return Robot{}, false, err
	}
	r.Tenant = model.TenantID(owner)
	return r, true, nil
}

// ---- ② 事故（キャッシュ化でガードが外れた）----

// FlatCache は「接続を減らす」ために入れた共有キャッシュの、よくある最初の形。
//
//   - Refresh はバックグラウンド goroutine から呼ばれ、ctx にテナントが無い。
//     repo.Tenant(t) が使えないので Unscoped で全テナントを読む（ガードの逃げ道を業務経路で使った）。
//   - キーは robot_id だけ。購読者の ctx は見ない。
type FlatCache struct {
	DB *repo.DB

	mu sync.RWMutex
	m  map[int64]Robot
}

func (c *FlatCache) Name() string { return "flat-cache" }

func (c *FlatCache) Refresh(ctx context.Context, _ []model.TenantID) error {
	rows, err := c.DB.Unscoped("subscription cache warm-up").Query(ctx, "sub.cache.all",
		`SELECT tenant_id, robot_id, name, ver FROM sub_robot ORDER BY tenant_id, robot_id LIMIT 1000`)
	if err != nil {
		return err
	}
	all, err := scanRobots(rows)
	if err != nil {
		return err
	}
	m := make(map[int64]Robot, len(all))
	for _, r := range all {
		m[r.ID] = r // ★キーにテナントが無い。後から読んだテナントが上書きする
	}
	c.mu.Lock()
	c.m = m
	c.mu.Unlock()
	return nil
}

func (c *FlatCache) Lookup(_ context.Context, robotID int64) (Robot, bool, error) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	r, ok := c.m[robotID] // ★購読者のテナントを見ていない。DB を通らないのでガードも通らない
	return r, ok, nil
}

// ---- ③ 修正後（接続は減らしたまま、ガードをキャッシュの境界まで持ってくる）----

// TenantHub はテナントごとに repo.Tenant(t) で1回読み、tenantcache（tenant 必須の2段キー）に載せる。
//
//   - 読み込みは「購読者が居るテナント」ごとに1本。購読者数ではなくテナント数に比例する。
//   - Lookup のテナントは購読者の ctx から取る。無ければエラー（全テナントにしない）。
//   - 自テナントに無い ID は「無い」。他テナントへフォールバックしない。
type TenantHub struct {
	DB *repo.DB

	once  sync.Once
	cache *tenantcache.Cache[Robot]
}

func (h *TenantHub) Name() string { return "tenant-hub" }

func (h *TenantHub) c() *tenantcache.Cache[Robot] {
	h.once.Do(func() { h.cache = tenantcache.New[Robot](0) })
	return h.cache
}

func (h *TenantHub) Refresh(ctx context.Context, tenants []model.TenantID) error {
	for _, t := range tenants {
		//smlint:allow loopquery 理由: テナントごとに1本（購読者ごとではない）。テナントを跨いで1本にまとめるとガードが外れる
		rows, err := h.DB.Tenant(t).Query(ctx, "sub.hub.tenant", selectTenant)
		if err != nil {
			return err
		}
		rs, err := scanRobots(rows)
		if err != nil {
			return err
		}
		h.c().EvictTenant(t) // 消えた robot を残さない（丸ごと差し替え）
		for _, r := range rs {
			h.c().Put(t, strconv.FormatInt(r.ID, 10), r)
		}
	}
	return nil
}

func (h *TenantHub) Lookup(ctx context.Context, robotID int64) (Robot, bool, error) {
	t, err := TenantFrom(ctx)
	if err != nil {
		return Robot{}, false, err
	}
	r, ok := h.c().Get(t, strconv.FormatInt(robotID, 10))
	return r, ok, nil
}
