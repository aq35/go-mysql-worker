// Package lint は EXP-9（保守性の自動検査）の実装。
//
// 目的は「レビューで気をつける」を減らすこと。
// これまでの実験で分かった事故のうち、**構文だけで見つかるもの**を機械に任せる。
//
// ★完全検出は謳わない。
// 各検査は「何を見ていて、何を見ていないか」を Doc に書き、
// 逃げ道（escape hatch）には理由の記述を要求して、使われた回数を数える。
package lint

import (
	"go/ast"
	"go/token"
	"strings"
	"sync"

	"golang.org/x/tools/go/analysis"
)

// Analyzers はこのパッケージが提供する検査。
func Analyzers() []*analysis.Analyzer {
	return []*analysis.Analyzer{
		RawDB, TxExternalCall, RowsAffected, MissingContext, LoopQuery, DomainTime, LayerImport, StructFill,
	}
}

// ---- 逃げ道（escape hatch）----

// AllowPrefix は逃げ道の書き方。理由が無いものは逃げ道として認めない。
//
//	//smlint:allow rawdb 理由: 移行中。2026-12 までに repo.Scope へ置き換える
//
// ★名前にハイフンを入れないこと。gofmt は `//名前:語` の形だけを
// ディレクティブとして扱い、それ以外には空白を入れて `// 名前 ...` に整形してしまう。
// 最初 `//sample-manual:allow` にしていたら、gofmt に空白を入れられて
// 逃げ道が効かなくなった（テストが落ちて気づいた）。
const AllowPrefix = "smlint:allow "

// escapes は、逃げ道が使われた回数（監査用）。
//
// ★ロックが要る。go/analysis は**パッケージごとに並列に**検査を走らせるので、
// ここは複数の goroutine から同時に書かれる。
// 最初これを素の map にしていて、リポジトリ全体へ当てたときに
// `fatal error: concurrent map writes` で落ちた。
// 逃げ道が少ないうちは（＝書き込みが稀なうちは）たまたま落ちなかった。
var (
	escapesMu sync.Mutex
	escapes   = map[string]int{}
)

// Escapes は逃げ道の使用回数を返す（実験で数えるため）。
func Escapes() map[string]int {
	escapesMu.Lock()
	defer escapesMu.Unlock()
	out := map[string]int{}
	for k, v := range escapes {
		out[k] = v
	}
	return out
}

// ResetEscapes は数え直す。
func ResetEscapes() {
	escapesMu.Lock()
	defer escapesMu.Unlock()
	escapes = map[string]int{}
}

func countEscape(rule string) {
	escapesMu.Lock()
	escapes[rule]++
	escapesMu.Unlock()
}

// allowed は、その位置に有効な逃げ道コメントがあるか。
//
// **理由の記述を必須にする。** 「うるさいから消した」を残さないため。
func allowed(pass *analysis.Pass, pos token.Pos, rule string) bool {
	file := fileFor(pass, pos)
	if file == nil {
		return false
	}
	line := pass.Fset.Position(pos).Line

	// 逃げ道は「その行・直前の行」か「囲んでいる関数の doc コメント」に書く。
	// 行だけに限ると、関数まるごとを許したい場合に書けない。
	funcStart, _ := enclosingFunc(file, pos)
	for _, cg := range file.Comments {
		cline := pass.Fset.Position(cg.End()).Line
		inDoc := funcStart.IsValid() && cg.End() < funcStart &&
			pass.Fset.Position(funcStart).Line-cline <= 3
		if cline != line && cline != line-1 && !inDoc {
			continue
		}
		for _, c := range cg.List {
			// 「//」を外し、空白を許して判定する（整形の揺れに強くする）
			text := strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(c.Text), "//"))
			if !strings.HasPrefix(text, AllowPrefix) {
				continue
			}
			rest := strings.TrimSpace(strings.TrimPrefix(text, AllowPrefix))
			if !strings.HasPrefix(rest, rule) {
				continue
			}
			reason := strings.TrimSpace(strings.TrimPrefix(rest, rule))
			if !strings.Contains(reason, "理由") || len(reason) < 8 {
				pass.Reportf(c.Pos(), "逃げ道には理由を書くこと: %s%s 理由: ...", AllowPrefix, rule)
				return false
			}
			countEscape(rule)
			return true
		}
	}
	return false
}

func fileFor(pass *analysis.Pass, pos token.Pos) *ast.File {
	for _, f := range pass.Files {
		if f.Pos() <= pos && pos <= f.End() {
			return f
		}
	}
	return nil
}

// IncludeTests は _test.go も検査対象にするか（既定は false）。
//
// ★テストコードは対象外にしている。実測（このリポジトリ）では、
// 指摘 191 件のうち 148 件がテストコードで、そのほとんどが
// 「後片付けの DELETE で影響行数を見ていない」「テスト用の短い書き方」だった。
// 本番コードの問題が埋もれるので、既定では外す。
var IncludeTests = false

func report(pass *analysis.Pass, rule string, pos token.Pos, format string, args ...any) {
	if !IncludeTests && isTestFile(pass, pos) {
		return
	}
	if allowed(pass, pos, rule) {
		return
	}
	pass.Reportf(pos, format, args...)
}

func isTestFile(pass *analysis.Pass, pos token.Pos) bool {
	name := pass.Fset.Position(pos).Filename
	return strings.HasSuffix(name, "_test.go")
}

// ---- 共通の判定 ----

// isDomainPackage は「DB を知らないはずの層」か。
// 名前で判断しているので、命名規約が違うプロジェクトでは設定を変える必要がある。
var domainMarkers = []string{"/domain", "/service", "/usecase", "/handler", "/http", "/api"}

func isDomainPackage(path string) bool {
	for _, m := range domainMarkers {
		if strings.Contains(path, m) {
			return true
		}
	}
	return false
}

// selectorName は x.Sel の名前（`db.QueryContext` なら QueryContext）。
func selectorName(e ast.Expr) string {
	sel, ok := e.(*ast.SelectorExpr)
	if !ok {
		return ""
	}
	return sel.Sel.Name
}

// receiverText は呼び出しのレシーバを文字列で返す（`db.Exec` なら "db"）。
func receiverText(e ast.Expr) string {
	sel, ok := e.(*ast.SelectorExpr)
	if !ok {
		return ""
	}
	id, ok := sel.X.(*ast.Ident)
	if !ok {
		return ""
	}
	return id.Name
}

// firstStringLit は引数の中の最初の文字列リテラル（SQL を見るため）。
func firstStringLit(args []ast.Expr) (string, bool) {
	for _, a := range args {
		if lit, ok := a.(*ast.BasicLit); ok && lit.Kind == token.STRING {
			return strings.Trim(lit.Value, "`\""), true
		}
		if id, ok := a.(*ast.Ident); ok && id.Obj != nil {
			if vs, ok := id.Obj.Decl.(*ast.ValueSpec); ok && len(vs.Values) == 1 {
				if lit, ok := vs.Values[0].(*ast.BasicLit); ok && lit.Kind == token.STRING {
					return strings.Trim(lit.Value, "`\""), true
				}
			}
		}
	}
	return "", false
}

// enclosingFunc は pos を含む関数の開始・終了位置。
func enclosingFunc(file *ast.File, pos token.Pos) (token.Pos, token.Pos) {
	var start, end token.Pos
	ast.Inspect(file, func(n ast.Node) bool {
		fd, ok := n.(*ast.FuncDecl)
		if !ok {
			return true
		}
		if fd.Pos() <= pos && pos <= fd.End() {
			start, end = fd.Pos(), fd.End()
		}
		return true
	})
	return start, end
}
