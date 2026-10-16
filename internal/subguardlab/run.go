package subguardlab

import (
	"context"
	"database/sql"
	"sync"
	"sync/atomic"
	"time"

	"github.com/aq35/sample_manual/internal/model"
)

// Subscriber は購読者1人。Tenant は認証済みの主体、RobotID は購読引数（クライアントが決める）。
// Owned=false は「自テナントに存在しない ID を指定した」購読（ID の総当たり・取り違え）。
type Subscriber struct {
	Tenant  model.TenantID
	RobotID int64
	Owned   bool
}

// Result は1方式ぶんの集計。
type Result struct {
	Deliveries int64 // 購読者に値を渡した回数
	CrossLeak  int64 // 他テナントの行を渡した回数（合計）
	// CrossCollision: 自テナントにも同じ ID があるのに、他テナントの値を渡した（キー衝突）
	CrossCollision int64
	// CrossProbe: 自テナントに無い ID で、他テナントの行を渡した（認可の素通り）
	CrossProbe int64
	NotFound   int64 // 「無い」と返した回数
	Errors     int64

	PeakInUse int   // poll 中に同時に使っていた接続の最大
	WaitCount int64 // プールが空かず待たされた回数
	Elapsed   time.Duration
}

// Run は rounds 回の poll を回す。各 poll で Refresh（テナント無しの ctx）→ 全購読者が同時に Lookup。
func Run(ctx context.Context, src Source, sqldb *sql.DB, subs []Subscriber, rounds int) Result {
	var res Result
	tenants := activeTenants(subs)

	before := sqldb.Stats()
	stop := make(chan struct{})
	var peak atomic.Int64
	var sampler sync.WaitGroup
	sampler.Add(1)
	go func() {
		defer sampler.Done()
		tk := time.NewTicker(100 * time.Microsecond)
		defer tk.Stop()
		for {
			if n := int64(sqldb.Stats().InUse); n > peak.Load() {
				peak.Store(n)
			}
			select {
			case <-stop:
				return
			case <-tk.C:
			}
		}
	}()

	start := time.Now()
	for i := 0; i < rounds; i++ {
		// バックグラウンドの poller。★リクエスト由来ではないので ctx にテナントは無い
		if err := src.Refresh(context.Background(), tenants); err != nil {
			res.Errors++
			continue
		}
		var wg sync.WaitGroup
		var deliveries, cross, coll, probe, nf, errs atomic.Int64
		for _, s := range subs {
			wg.Add(1)
			go func(s Subscriber) {
				defer wg.Done()
				r, ok, err := src.Lookup(WithTenant(ctx, s.Tenant), s.RobotID)
				switch {
				case err != nil:
					errs.Add(1)
				case !ok:
					nf.Add(1)
				default:
					deliveries.Add(1)
					if r.Tenant != s.Tenant {
						cross.Add(1)
						if s.Owned {
							coll.Add(1)
						} else {
							probe.Add(1)
						}
					}
				}
			}(s)
		}
		wg.Wait()
		res.Deliveries += deliveries.Load()
		res.CrossLeak += cross.Load()
		res.CrossCollision += coll.Load()
		res.CrossProbe += probe.Load()
		res.NotFound += nf.Load()
		res.Errors += errs.Load()
	}
	res.Elapsed = time.Since(start)
	close(stop)
	sampler.Wait()
	res.PeakInUse = int(peak.Load())
	res.WaitCount = sqldb.Stats().WaitCount - before.WaitCount
	return res
}

func activeTenants(subs []Subscriber) []model.TenantID {
	seen := map[model.TenantID]bool{}
	var out []model.TenantID
	for _, s := range subs {
		if !seen[s.Tenant] {
			seen[s.Tenant] = true
			out = append(out, s.Tenant)
		}
	}
	return out
}
