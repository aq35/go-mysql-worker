package structfilllab_test

// EXP-64: 構造体のセット漏れを機械で止める。
//
//	go test ./internal/structfilllab/ -run TestEXP64 -v
//
// 項目を1つ足したとき、作る場所の直し忘れは Go のコンパイラでは止まらない（ゼロ値のまま通る）。
// 作り方4種（直接リテラル / オプション関数 / 引数 / 引数用 struct）で、
// 「コンパイラ」「structfill 検査」「ラウンドトリップテスト」がそれぞれ何箇所を止めるかを数える。
// MySQL は使わない。

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/aq35/sample_manual/internal/expkit"
	"github.com/aq35/sample_manual/internal/structfilllab"
)

const sites = 10 // 作る場所の数（呼ぶ側の関数の数）

func TestEXP64_構造体のセット漏れ(t *testing.T) {
	if testing.Short() {
		t.Skip("go build と検査コマンドを何度も走らせるので -short では飛ばす")
	}
	ctx := context.Background()
	rec := expkit.NewRecorder("EXP-64", "struct-fill",
		"項目を足したときの『作る場所の直し忘れ』を、コンパイラ・検査・テストのどれが止めるか")
	rec.Env(expkit.CaptureEnv(ctx, nil))
	rec.Freeze(strings.Join([]string{
		"1) 直接リテラル・オプション関数・引数用 struct は、項目を足してもコンパイルが通る（直し忘れ 10/10 が素通り）。",
		"2) 必須の値を引数にした形だけは、呼ぶ側 10/10 がコンパイルエラーになる。",
		"3) structfill 検査（宣言した型だけ見る）は、直接リテラルと引数用 struct の 10/10 を止める。",
		"   オプション関数は呼ぶ側を止められない（作る場所が関数呼び出しで、リテラルではない）。",
		"4) 変換関数の写し忘れは、手書きのテストデータでは通り、全項目を自動で埋めたラウンドトリップで落ちる。",
		"5) 宣言の無い型まで全部見ると、このリポジトリでも指摘が多すぎて使えない（だから宣言した型だけにする）。",
	}, " "))
	rec.Workload("sites", sites)
	rec.Workload("change", "Schedule に PlannedQty int を足す。呼ぶ側は直さない")

	bin := buildLinter(t)

	// ---- A. 作り方4種 × 止める仕組み ----
	type row struct {
		style                       structfilllab.Style
		compileErrs                 int
		lintCallers, lintModel      int
		caughtCompile, caughtWithLn int
	}
	var rows []row
	for _, st := range structfilllab.Styles {
		// 足す前はどれもきれいに通ること（測定器の確認）
		before := t.TempDir()
		if err := structfilllab.WriteModule(before, st, sites, false, true); err != nil {
			t.Fatal(err)
		}
		if n := len(compileErrors(t, before)); n != 0 {
			t.Fatalf("%s: 足す前にコンパイルエラー %d", st, n)
		}
		if f := runLint(t, bin, before, false); len(f) != 0 {
			t.Fatalf("%s: 足す前に指摘 %v", st, f)
		}

		after := t.TempDir()
		if err := structfilllab.WriteModule(after, st, sites, true, true); err != nil {
			t.Fatal(err)
		}
		errs := compileErrors(t, after)
		r := row{style: st, compileErrs: len(errs)}
		errLines := linesIn(errs, "callers/")
		r.caughtCompile = len(errLines)
		if len(errs) == 0 {
			// コンパイルが通るときだけ検査を当てられる
			for _, f := range runLint(t, bin, after, false) {
				if strings.Contains(f, "callers/") {
					r.lintCallers++
				} else {
					r.lintModel++
				}
			}
		}
		r.caughtWithLn = r.caughtCompile + r.lintCallers
		rows = append(rows, r)
		t.Logf("%-10s compile_errors=%d lint(callers)=%d lint(model)=%d 素通り=%d/%d",
			st, r.compileErrs, r.lintCallers, r.lintModel, sites-r.caughtWithLn, sites)
	}
	for _, r := range rows {
		silentCompileOnly := sites - r.caughtCompile
		silentWithLint := sites - r.caughtWithLn
		rec.Add(expkit.Variant{
			Name:     "作り方: " + string(r.style),
			Desc:     styleDesc(r.style),
			Accident: silentWithLint > 0,
			Counters: map[string]int64{
				"sites":                    sites,
				"compile_errors":           int64(r.compileErrs),
				"lint_findings_callers":    int64(r.lintCallers),
				"lint_findings_model":      int64(r.lintModel),
				"silent_compile_only":      int64(silentCompileOnly),
				"silent_compile_plus_lint": int64(silentWithLint),
			},
			Notes: []string{fmt.Sprintf(
				"コンパイラだけ: %d/%d が素通り。コンパイラ＋structfill: %d/%d が素通り（model 側の指摘 %d）",
				silentCompileOnly, sites, silentWithLint, sites, r.lintModel)},
		})
	}

	// ---- B. 変換関数の写し忘れ：手書きデータ vs 全項目自動 ----
	at := time.Date(2026, 10, 7, 9, 0, 0, 0, time.UTC)
	hand := structfilllab.Schedule{ID: 1, TenantID: "t1", PlannedAt: at} //smlint:allow structfill 理由: 「よくある手書きのテストデータ」を再現するため、わざと一部だけ埋める
	var auto structfilllab.Schedule
	if err := structfilllab.FillNonZero(&auto); err != nil {
		t.Fatal(err)
	}
	type mapper struct {
		name   string
		to     func(structfilllab.Schedule) structfilllab.ScheduleRow
		broken bool
	}
	mappers := []mapper{
		{"正しい変換", structfilllab.ToRow, false},
		{"PlannedQty の写し忘れ", structfilllab.ToRowForgot, true},
		{"PlannedAt と ActualAt の取り違え", structfilllab.ToRowSwapped, true},
	}
	var handMissed, autoMissed int64
	for _, m := range mappers {
		dHand := structfilllab.DiffFields(hand, structfilllab.FromRow(m.to(hand)))
		dAuto := structfilllab.DiffFields(auto, structfilllab.FromRow(m.to(auto)))
		if m.name == "PlannedQty の写し忘れ" {
			// 戻す側も忘れている、の組み合わせ
			dHand = structfilllab.DiffFields(hand, structfilllab.FromRowForgot(m.to(hand)))
			dAuto = structfilllab.DiffFields(auto, structfilllab.FromRowForgot(m.to(auto)))
		}
		if m.broken && len(dHand) == 0 {
			handMissed++
		}
		if m.broken && len(dAuto) == 0 {
			autoMissed++
		}
		if !m.broken && (len(dHand) != 0 || len(dAuto) != 0) {
			t.Fatalf("正しい変換で差が出た（測定器の誤り）: hand=%v auto=%v", dHand, dAuto)
		}
		rec.Add(expkit.Variant{
			Name:     "ラウンドトリップ: " + m.name,
			Accident: m.broken && len(dAuto) == 0,
			Notes: []string{
				fmt.Sprintf("手書きのテストデータ（ID・TenantID・PlannedAt だけ）: 差 %v → %s", dHand, verdict(m.broken, dHand)),
				fmt.Sprintf("FillNonZero で全項目を埋めたデータ: 差 %v → %s", dAuto, verdict(m.broken, dAuto)),
			},
		})
		t.Logf("%s: hand=%v auto=%v", m.name, dHand, dAuto)
	}
	rec.Add(expkit.Variant{
		Name: "ラウンドトリップのまとめ",
		Counters: map[string]int64{
			"broken_mappers":        2,
			"missed_by_hand_data":   handMissed,
			"missed_by_fillnonzero": autoMissed,
		},
	})

	// ---- C. 宣言なしで全部の型を見たら（このリポジトリ） ----
	root, _ := filepath.Abs("../..")
	all := runLint(t, bin, root, true)
	types := map[string]int{}
	re := regexp.MustCompile(`: (\S+) の項目が埋まっていない`)
	for _, f := range all {
		if m := re.FindStringSubmatch(f); m != nil {
			types[m[1]]++
		}
	}
	marked := runLint(t, bin, root, false)
	var top []string
	for k, v := range types {
		top = append(top, fmt.Sprintf("%s=%d", k, v))
	}
	sort.Slice(top, func(i, j int) bool { return countOf(top[i]) > countOf(top[j]) })
	if len(top) > 8 {
		top = top[:8]
	}
	rec.Add(expkit.Variant{
		Name:     "このリポジトリ: 宣言なしで全部の型を見た（-structfill.all）",
		Accident: true,
		Counters: map[string]int64{"findings": int64(len(all)), "distinct_types": int64(len(types))},
		Notes:    []string{"指摘の多い型: " + strings.Join(top, ", ")},
	})
	rec.Add(expkit.Variant{
		Name:     "このリポジトリ: 宣言した型だけを見た（既定）",
		Counters: map[string]int64{"findings": int64(len(marked))},
		Notes:    []string{"宣言しているのは internal/structfilllab の Schedule / ScheduleRow だけ。事故側の再現コードは理由つきの逃げ道"},
	})
	t.Logf("全部の型: %d 件（%d 型） / 宣言した型だけ: %d 件", len(all), len(types), len(marked))

	rec.Scope(
		"Go の構造体リテラルとコンストラクタ。ORM が反射で埋める型（GORM の Create など）は対象外",
		"検査は『リテラルで作る場所』だけを見る。作った後の代入（s.X = ...）は見ない",
	)
	rec.Uncertain(
		"合成コードで数えた。実際のコードは作り方が混ざるので、素通りの数は混ざり具合で変わる",
		"引数が多い（6個以上）ときの順番の取り違えは、型が同じだとコンパイラも検査も止めない。Plan/Actual のように型を分けるか、引数用 struct＋検査にする",
	)
	rec.Artifact(
		"internal/lint/structfill.go: //smlint:structfill を付けた型の項目漏れを見る検査（sqllint に同梱）",
		"internal/structfilllab/fill.go: FillNonZero / DiffFields（reflect だけ。他プロジェクトへそのまま持って行ける）",
		"docs/struct-fill.md: 判断の表と導入の順番",
	)
	files, err := rec.Save(
		"項目を足しても、直接リテラル・オプション関数・引数用 struct はコンパイルが通り、作る場所の直し忘れは素通りする。" +
			"コンパイラが止めるのは『必須の値を引数にした形』だけ。リテラルは structfill 検査（宣言した型だけ）で止まり、" +
			"変換関数の写し忘れは全項目を自動で埋めたラウンドトリップテストで止まる（手書きのテストデータでは通る）。" +
			"オプション関数は任意の項目専用にする。全部の型を見ると騒がしすぎるので、守りたい型にだけ宣言を付ける。")
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("結果: %v", files)
}

func styleDesc(s structfilllab.Style) string {
	switch s {
	case structfilllab.Literal:
		return "呼ぶ側が model.Schedule{ID: 1, PlannedAt: at} と直接書く"
	case structfilllab.Options:
		return "NewSchedule(id, at, opts...)。足した項目は WithPlannedQty で渡す（渡し忘れても通る）"
	case structfilllab.Positional:
		return "NewSchedule(id, at, qty)。足した項目は引数に足す"
	case structfilllab.Params:
		return "NewSchedule(ScheduleParams{...})。足した項目は Params に足す"
	}
	return ""
}

func verdict(broken bool, diff []string) string {
	switch {
	case !broken && len(diff) == 0:
		return "通る（正しい）"
	case broken && len(diff) == 0:
		return "★通ってしまう（見逃し）"
	default:
		return "落ちる（検出）"
	}
}

func countOf(s string) int {
	var n int
	_, _ = fmt.Sscanf(s[strings.LastIndex(s, "=")+1:], "%d", &n)
	return n
}

var diagLine = regexp.MustCompile(`\.go:\d+:\d+: `)

func compileErrors(t *testing.T, dir string) []string {
	t.Helper()
	cmd := exec.Command("go", "build", "-gcflags=all=-e", "./...")
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), "GOWORK=off", "GOFLAGS=")
	out, _ := cmd.CombinedOutput()
	return matching(string(out))
}

func runLint(t *testing.T, bin, dir string, all bool) []string {
	t.Helper()
	args := []string{"-structfill"}
	if all {
		args = append(args, "-structfill.all")
	}
	args = append(args, "./...")
	cmd := exec.Command(bin, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), "GOWORK=off", "GOFLAGS=")
	out, _ := cmd.CombinedOutput() // 指摘があれば終了コードは非0
	var res []string
	for _, l := range matching(string(out)) {
		if strings.Contains(l, "項目が埋まっていない") {
			res = append(res, l)
		}
	}
	return res
}

func matching(out string) []string {
	var res []string
	for _, l := range strings.Split(out, "\n") {
		if diagLine.MatchString(l) {
			res = append(res, strings.TrimSpace(l))
		}
	}
	return res
}

func linesIn(lines []string, sub string) []string {
	var res []string
	for _, l := range lines {
		if strings.Contains(l, sub) {
			res = append(res, l)
		}
	}
	return res
}

func buildLinter(t *testing.T) string {
	t.Helper()
	bin := filepath.Join(t.TempDir(), "sqllint")
	cmd := exec.Command("go", "build", "-o", bin, "github.com/aq35/sample_manual/cmd/sqllint")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("検査コマンドをビルドできない: %v\n%s", err, out)
	}
	return bin
}
