// Package structfilllab は EXP-64（構造体のセット漏れを機械で止める）の実験コード。
//
// ここにある FillNonZero / DiffFields は reflect だけで書いてあり、
// どのプロジェクトにもそのまま持って行ける（変換関数のラウンドトリップテスト用）。
package structfilllab

import (
	"fmt"
	"reflect"
	"time"
)

// FillNonZero は、ptr が指す構造体の公開項目すべてにゼロでない値を入れる。
//
// ★手で書いたテストデータは「書いた項目」しか確かめない。項目を足しても、
// テストデータに足し忘れると、変換関数の写し忘れはゼロ値どうしの比較で通ってしまう。
// 全項目を自動で埋めれば、新しい項目も勝手に確かめる対象に入る。
//
// 値は項目ごとに変える（seed を1ずつ進める）。同じ型の項目を取り違えた変換
// （PlannedAt と ActualAt の入れ違いなど）も見つけるため。
func FillNonZero(ptr any) error {
	v := reflect.ValueOf(ptr)
	if v.Kind() != reflect.Pointer || v.Elem().Kind() != reflect.Struct {
		return fmt.Errorf("構造体へのポインタを渡すこと: %T", ptr)
	}
	seed := 1
	return fill(v.Elem(), &seed)
}

var timeType = reflect.TypeOf(time.Time{})

func fill(v reflect.Value, seed *int) error {
	n := *seed
	*seed++
	switch {
	case v.Type() == timeType:
		v.Set(reflect.ValueOf(time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC).Add(time.Duration(n) * time.Hour)))
		return nil
	}
	switch v.Kind() {
	case reflect.Bool:
		v.SetBool(true)
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		v.SetInt(int64(n))
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		v.SetUint(uint64(n))
	case reflect.Float32, reflect.Float64:
		v.SetFloat(float64(n) + 0.5)
	case reflect.String:
		v.SetString(fmt.Sprintf("v%d", n))
	case reflect.Pointer:
		p := reflect.New(v.Type().Elem())
		if err := fill(p.Elem(), seed); err != nil {
			return err
		}
		v.Set(p)
	case reflect.Slice:
		s := reflect.MakeSlice(v.Type(), 1, 1)
		if err := fill(s.Index(0), seed); err != nil {
			return err
		}
		v.Set(s)
	case reflect.Struct:
		for i := 0; i < v.NumField(); i++ {
			if !v.Type().Field(i).IsExported() {
				continue
			}
			if err := fill(v.Field(i), seed); err != nil {
				return err
			}
		}
	default:
		return fmt.Errorf("この型は埋められない: %s", v.Type())
	}
	return nil
}

// DiffFields は、a と b（同じ構造体型）で値が違う公開項目の名前を返す。
func DiffFields(a, b any) []string {
	va, vb := reflect.ValueOf(a), reflect.ValueOf(b)
	var out []string
	for i := 0; i < va.NumField(); i++ {
		f := va.Type().Field(i)
		if !f.IsExported() {
			continue
		}
		if !reflect.DeepEqual(va.Field(i).Interface(), vb.Field(i).Interface()) {
			out = append(out, f.Name)
		}
	}
	return out
}
