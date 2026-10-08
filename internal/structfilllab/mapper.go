package structfilllab

import "time"

// Schedule は業務側の型。PlannedQty は「後から足した項目」という設定。
//
//smlint:structfill
type Schedule struct {
	ID         int64
	TenantID   string
	PlannedAt  time.Time
	PlannedQty int // ← 後から足した
	ActualAt   *time.Time
}

// ScheduleRow は DB の行。
//
//smlint:structfill
type ScheduleRow struct {
	ID         int64
	TenantID   string
	PlannedAt  time.Time
	PlannedQty int
	ActualAt   *time.Time
}

// ToRowForgot / FromRowForgot は、PlannedQty を足したときに直し忘れた変換（事故の再現）。
func ToRowForgot(s Schedule) ScheduleRow {
	return ScheduleRow{ID: s.ID, TenantID: s.TenantID, PlannedAt: s.PlannedAt, ActualAt: s.ActualAt} //smlint:allow structfill 理由: EXP-64 で写し忘れを再現するための事故側
}

func FromRowForgot(r ScheduleRow) Schedule {
	return Schedule{ID: r.ID, TenantID: r.TenantID, PlannedAt: r.PlannedAt, ActualAt: r.ActualAt} //smlint:allow structfill 理由: EXP-64 で写し忘れを再現するための事故側
}

// ToRow / FromRow は正しい変換。
func ToRow(s Schedule) ScheduleRow {
	return ScheduleRow{ID: s.ID, TenantID: s.TenantID, PlannedAt: s.PlannedAt, PlannedQty: s.PlannedQty, ActualAt: s.ActualAt}
}

func FromRow(r ScheduleRow) Schedule {
	return Schedule{ID: r.ID, TenantID: r.TenantID, PlannedAt: r.PlannedAt, PlannedQty: r.PlannedQty, ActualAt: r.ActualAt}
}

// ToRowSwapped は、同じ型の項目を取り違えた変換（PlannedAt に ActualAt を入れる）。
func ToRowSwapped(s Schedule) ScheduleRow {
	r := ToRow(s)
	if s.ActualAt != nil {
		r.PlannedAt = *s.ActualAt
	}
	return r
}
