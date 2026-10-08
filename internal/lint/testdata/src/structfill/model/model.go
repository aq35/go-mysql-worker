package model

import (
	"errors"
	"time"
)

//smlint:structfill
type Schedule struct { // want Schedule:"structfill"
	ID         int64
	PlannedAt  time.Time
	PlannedQty int
	Note       string //smlint:optional
	version    int
}

// 宣言していない型は見ない
type Loose struct {
	A int
	B int
}

func NewSchedule(id int64, at time.Time) Schedule {
	return Schedule{ID: id, PlannedAt: at} // want `Schedule の項目が埋まっていない: PlannedQty, version`
}

func Full(id int64, at time.Time, qty int) Schedule {
	return Schedule{ID: id, PlannedAt: at, PlannedQty: qty, version: 1}
}

func Positional(id int64, at time.Time, qty int) Schedule {
	return Schedule{id, at, qty, "", 1} // 名前なしはコンパイラが全項目を要求する
}

func Parse(s string) (Schedule, error) {
	if s == "" {
		return Schedule{}, errors.New("empty") // エラーと一緒に返す空リテラルは除く
	}
	return Schedule{}, nil // want `Schedule の項目が埋まっていない`
}

func loose() Loose { return Loose{A: 1} }

//smlint:allow structfill 理由: 移行中。2026-12 までに NewSchedule の引数へ寄せる
func Legacy(id int64) Schedule {
	return Schedule{ID: id}
}
