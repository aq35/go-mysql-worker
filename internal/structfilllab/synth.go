package structfilllab

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Style は構造体の作り方。EXP-64 では「項目を1つ足したとき、作る場所の直し忘れを誰が止めるか」を比べる。
type Style string

const (
	Literal    Style = "literal"    // 呼ぶ側が model.Schedule{...} と直接書く
	Options    Style = "options"    // NewSchedule(id, at, opts...)。新しい項目は WithPlannedQty で渡す
	Positional Style = "positional" // NewSchedule(id, at, qty)。新しい項目は引数に足す
	Params     Style = "params"     // NewSchedule(ScheduleParams{...})。新しい項目は Params に足す
)

// Styles は比べる順。
var Styles = []Style{Literal, Options, Positional, Params}

// WriteModule は、作る場所が sites 箇所ある小さなモジュールを dir に書く。
//
// after=false は項目を足す前、after=true は PlannedQty を足した後（呼ぶ側は**直していない**）。
// marked=true なら型に //smlint:structfill を付ける。
func WriteModule(dir string, style Style, sites int, after, marked bool) error {
	files := map[string]string{
		"go.mod":             "module synth\n\ngo 1.26\n",
		"model/model.go":     modelSource(style, after, marked),
		"callers/callers.go": callersSource(style, sites),
	}
	for name, src := range files {
		p := filepath.Join(dir, name)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(p, []byte(src), 0o644); err != nil {
			return err
		}
	}
	return nil
}

func modelSource(style Style, after, marked bool) string {
	mark := ""
	if marked {
		mark = "//smlint:structfill\n"
	}
	qtyField := ""
	if after {
		qtyField = "\tPlannedQty int\n"
	}
	var b strings.Builder
	b.WriteString("package model\n\nimport \"time\"\n\n")
	if style != Params {
		b.WriteString(mark)
	}
	fmt.Fprintf(&b, "type Schedule struct {\n\tID        int64\n\tPlannedAt time.Time\n%s}\n\n", qtyField)

	switch style {
	case Literal:
	case Options:
		b.WriteString("type Option func(*Schedule)\n\n")
		b.WriteString("func NewSchedule(id int64, at time.Time, opts ...Option) Schedule {\n" +
			"\ts := Schedule{ID: id, PlannedAt: at}\n\tfor _, o := range opts {\n\t\to(&s)\n\t}\n\treturn s\n}\n")
		if after {
			b.WriteString("\nfunc WithPlannedQty(q int) Option { return func(s *Schedule) { s.PlannedQty = q } }\n")
		}
	case Positional:
		if after {
			b.WriteString("func NewSchedule(id int64, at time.Time, qty int) Schedule {\n" +
				"\treturn Schedule{ID: id, PlannedAt: at, PlannedQty: qty}\n}\n")
		} else {
			b.WriteString("func NewSchedule(id int64, at time.Time) Schedule {\n" +
				"\treturn Schedule{ID: id, PlannedAt: at}\n}\n")
		}
	case Params:
		b.WriteString(mark)
		fmt.Fprintf(&b, "type ScheduleParams struct {\n\tID        int64\n\tPlannedAt time.Time\n%s}\n\n", qtyField)
		set := ""
		if after {
			set = ", PlannedQty: p.PlannedQty"
		}
		fmt.Fprintf(&b, "func NewSchedule(p ScheduleParams) Schedule {\n\treturn Schedule{ID: p.ID, PlannedAt: p.PlannedAt%s}\n}\n", set)
	}
	return b.String()
}

func callersSource(style Style, sites int) string {
	var b strings.Builder
	b.WriteString("package callers\n\nimport (\n\t\"time\"\n\n\t\"synth/model\"\n)\n\n")
	for i := 1; i <= sites; i++ {
		var expr string
		switch style {
		case Literal:
			expr = fmt.Sprintf("model.Schedule{ID: %d, PlannedAt: at}", i)
		case Options, Positional:
			expr = fmt.Sprintf("model.NewSchedule(%d, at)", i)
		case Params:
			expr = fmt.Sprintf("model.NewSchedule(model.ScheduleParams{ID: %d, PlannedAt: at})", i)
		}
		fmt.Fprintf(&b, "func Site%d(at time.Time) model.Schedule { return %s }\n\n", i, expr)
	}
	return b.String()
}
