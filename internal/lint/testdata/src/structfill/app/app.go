package app

import (
	"time"

	"structfill/model"
)

func Use(at time.Time) []model.Schedule {
	return []model.Schedule{
		{ID: 1, PlannedAt: at, PlannedQty: 3},  // 別パッケージからは非公開項目（version）は書けないので見ない
		{ID: 2, PlannedAt: at},                 // want `Schedule の項目が埋まっていない: PlannedQty`
		*&model.Schedule{ID: 3, PlannedQty: 1}, // want `Schedule の項目が埋まっていない: PlannedAt`
	}
}
