package lint_test

// EXP-9: 保守性の自動検査。
//
//	go test ./internal/lint/ -run TestEXP9 -v
//
// 「レビューで気をつける」を減らすために、これまでの実験で分かった事故のうち
// 構文で見つかるものを機械に任せる。**完全検出は謳わない。**

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"golang.org/x/tools/go/analysis"
	"golang.org/x/tools/go/analysis/analysistest"

	"github.com/aq35/sample_manual/internal/expkit"
	"github.com/aq35/sample_manual/internal/lint"
)

// ---- 検査そのものの正しさ（analysistest） ----

func TestAnalyzers_想定どおり検出する(t *testing.T) {
	dir := analysistest.TestData()
	cases := []struct {
		a   *analysis.Analyzer
		pkg string
	}{
		{lint.RawDB, "rawdb/domain"},
		{lint.DomainTime, "domaintime/domain"},
		{lint.MissingContext, "nocontext/data"},
		{lint.RowsAffected, "rowsaffected/data"},
		{lint.LoopQuery, "loopquery/data"},
		{lint.TxExternalCall, "txhttp/data"},
		{lint.LayerImport, "layerimport/repo"},
		{lint.StructFill, "structfill/model"},
		{lint.StructFill, "structfill/app"},
	}
	for _, c := range cases {
		t.Run(c.a.Name, func(t *testing.T) {
			analysistest.Run(t, dir, c.a, c.pkg)
		})
	}
}

func TestEscapeHatch_理由が要る(t *testing.T) {
	lint.ResetEscapes()
	analysistest.Run(t, analysistest.TestData(), lint.DomainTime, "domaintime/domain")
	esc := lint.Escapes()
	if esc["domaintime"] == 0 {
		t.Errorf("理由つきの逃げ道が数えられていない: %v", esc)
	}
	t.Logf("逃げ道の使用回数（監査に残す値）: %v", esc)
}

// ---- このリポジトリに当ててみる ----

func TestEXP9_保守性の自動検査(t *testing.T) {
	ctx := context.Background()
	rec := expkit.NewRecorder("EXP-9", "static-analysis",
		"実験で分かった事故のうち、構文で見つかるものを go/analysis で検出する")
	rec.Env(expkit.CaptureEnv(ctx, nil))
	rec.Freeze(strings.Join([]string{
		"1) 生の *sql.DB の受け渡し、トランザクション内の外部呼び出し、影響行数の捨て、",
		"   context 無しの DB 呼び出し、ループ内クエリ、業務ロジック内の time.Now() は、",
		"   構文だけで検出できる。",
		"2) unmanaged goroutine と unbounded channel は、構文だけでは判定できない（検出しない）。",
		"3) 逃げ道は塞がず、理由を必須にして使用回数を数える。",
	}, " "))

	// 実際にこのリポジトリへ当てる
	bin := buildLinter(t)
	findings := runLinter(t, bin, "./...")

	byRule := map[string]int64{}
	for _, f := range findings {
		byRule[f.rule]++
	}
	var notes []string
	for _, f := range findings {
		notes = append(notes, fmt.Sprintf("%s: %s", f.rule, f.line))
	}
	if len(notes) == 0 {
		notes = append(notes, "このリポジトリでは指摘なし")
	}

	escapes := countEscapeHatches(t)
	byRule["escape_hatches"] = int64(escapes)
	rec.Add(expkit.Variant{
		Name:     "このリポジトリ全体に当てた結果（対応後）",
		Desc:     "go run ./cmd/sqllint ./...（_test.go は対象外）",
		Counters: byRule,
		Notes:    append(notes, fmt.Sprintf("理由つきの逃げ道 %d 箇所（すべて grep で一覧できる）", escapes)),
	})
	rec.Add(expkit.Variant{
		Name: "対応前（測定の記録）",
		Desc: "検査を作った直後に、このリポジトリへ当てたときの数字",
		Counters: map[string]int64{
			"all_files":           191,
			"non_test_files":      43,
			"after_loopquery_fix": 27,
			"real_bug_found":      1,
		},
		Accident: true,
		Notes: []string{
			"191 件のうち 148 件はテストコード。テストの後片付けなどで埋もれるため、_test.go は対象外にした",
			"残る 43 件のうち 16 件は『ループ変数が SQL 文そのもの』という誤検出だった" +
				"（for _, stmt := range statements { db.Exec(stmt) }）。" +
				"ループ変数が束縛値として使われている場合だけ指摘するように直して 27 件へ",
			"★27 件の中に本物が 1 件あった: repo.Migrate の完了記録が影響行数を見ていなかった。" +
				"記録できていないのに『完了』と思い込むと、次の起動が黙って先へ進む。修正済み",
			"残りは実験コードそのもの（測定対象）で、理由つきの逃げ道を入れた",
			"★あとから分かった検査自身のバグ: 逃げ道の使用回数を数える map にロックが無く、" +
				"EXP-10 のコードを足して逃げ道が増えたところで " +
				"`fatal error: concurrent map writes` で落ちた。" +
				"go/analysis はパッケージごとに並列で走る。" +
				"**逃げ道が少ないうちは書き込みが稀で、たまたま落ちていなかった**",
		},
	})

	// 検出できるもの／できないものを明記する
	rec.Add(expkit.Variant{
		Name: "検出できるもの（実装した検査）",
		Notes: []string{
			"rawdb: domain/service/handler へ生の *sql.DB を渡す（引数・構造体フィールド）",
			"txhttp: Tx/WithTx/Transaction に渡した関数リテラルの中の HTTP 呼び出し",
			"rowsaffected: UPDATE/DELETE の Exec の戻り値を捨てている（SQL が定数のとき）",
			"nocontext: Exec/Query/QueryRow/Begin/Prepare（Context の付かない版）",
			"loopquery: for / range の中の Query/QueryRow/Exec",
			"domaintime: domain/service 層での time.Now()",
		},
	})
	rec.Add(expkit.Variant{
		Name:     "検出しないと決めたもの（理由つき）",
		Accident: false,
		Notes: []string{
			"unmanaged goroutine: 「管理されている」かどうかは構文で判定できない。" +
				"WaitGroup/errgroup を使っていても、待っているとは限らない。誤検出が多すぎる",
			"unbounded channel: バッファ無しチャネルは無制限ではない。" +
				"本当の危険はスライスを溜め続けるコードだが、これは構文では見分けられない（EXP-4 の測定で見るべきもの）",
			"migration file の変更: 静的解析ではなく checksum で検出している（EXP-6）。" +
				"適用済みの .sql が書き換わったら起動時に止まる",
			"scope 無し repository 呼び出し: 型で禁止できている（*repo.Scope しか受け取らない）。" +
				"検査を足すより、型で表現できるならそちらが確実",
		},
	})

	rec.Scope(
		"Go の構文と型情報だけを見る（go/analysis）。実行時の振る舞いは見ていない",
		"『domain 層』の判定はパッケージパスの文字列（/domain /service /usecase /handler /http /api）",
		"逃げ道は //smlint:allow <rule> 理由: ... の形。理由が無いと逃げ道として認めない",
	)
	rec.Uncertain(
		"interface に包んで渡された *sql.DB は検出できない（rawdb）",
		"別関数に切り出された HTTP 呼び出しは検出できない（txhttp）。関数をまたぐ追跡はしていない",
		"動的に組み立てた SQL は判定できない（rowsaffected）",
		"呼び出し先の中にあるクエリは検出できない（loopquery）",
		"false positive の実測は、このリポジトリ（1件も出ない状態）でしか行っていない。"+
			"他のプロジェクトに当てたときの誤検出率は未測定",
	)
	rec.Artifact(
		"internal/lint: 6つの検査。Doc に「何を見て、何を見ていないか」を書いてある",
		"cmd/sqllint: multichecker。go vet と同じ使い勝手で ./... に当てられる",
		"逃げ道の監査: lint.Escapes() で理由つきの逃げ道が何回使われたかを数えられる",
	)
	rec.Next("EXP-10 SQLite companion")

	files, err := rec.Save(strings.Join([]string{
		fmt.Sprintf("6つの検査を実装し、このリポジトリ全体で指摘は %d 件だった。", len(findings)),
		"unmanaged goroutine と unbounded channel は、構文だけでは判定できないので実装していない",
		"（できないものを「できる」と言わないことが、この検査を信用に足るものにする）。",
	}, ""))
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("検査の指摘 %d 件 / 結果: %v", len(findings), files)
	for _, f := range findings {
		t.Logf("  %s", f.line)
	}
}

type finding struct {
	rule string
	line string
}

func buildLinter(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	bin := filepath.Join(dir, "sqllint")
	cmd := exec.Command("go", "build", "-o", bin, "github.com/aq35/sample_manual/cmd/sqllint")
	cmd.Env = os.Environ()
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("検査コマンドをビルドできない: %v\n%s", err, out)
	}
	return bin
}

func runLinter(t *testing.T, bin, target string) []finding {
	t.Helper()
	root, err := filepath.Abs("../..")
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(bin, target)
	cmd.Dir = root
	out, _ := cmd.CombinedOutput() // 指摘があれば終了コードは非0

	var findings []finding
	for _, line := range strings.Split(string(out), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		// 形式: path:line:col: メッセージ
		parts := strings.SplitN(line, ": ", 2)
		if len(parts) != 2 {
			continue
		}
		findings = append(findings, finding{rule: ruleOf(parts[1]), line: line})
	}
	return findings
}

func ruleOf(msg string) string {
	switch {
	case strings.Contains(msg, "*sql.DB"):
		return "rawdb"
	case strings.Contains(msg, "トランザクションの中で外部"):
		return "txhttp"
	case strings.Contains(msg, "影響行数"), strings.Contains(msg, "結果をまったく見ていない"):
		return "rowsaffected"
	case strings.Contains(msg, "context を渡していない"):
		return "nocontext"
	case strings.Contains(msg, "ループの中で"):
		return "loopquery"
	case strings.Contains(msg, "time.Now()"):
		return "domaintime"
	case strings.Contains(msg, "項目が埋まっていない"):
		return "structfill"
	}
	return "other"
}

// countEscapeHatches は理由つきの逃げ道の数（監査に残す値）。
func countEscapeHatches(t *testing.T) int {
	t.Helper()
	root, err := filepath.Abs("../..")
	if err != nil {
		t.Fatal(err)
	}
	out, err := exec.Command("grep", "-rn", "--include=*.go", "smlint:allow", root).Output()
	if err != nil {
		return 0
	}
	n := 0
	for _, line := range strings.Split(string(out), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		// 定義そのもの（internal/lint と testdata）は数えない
		if strings.Contains(line, "/internal/lint/") {
			continue
		}
		n++
	}
	return n
}
