package lint

import (
	"go/ast"
	"go/types"
	"sort"
	"strings"

	"golang.org/x/tools/go/analysis"
)

// StructFill: 「全項目を必ず埋める」と宣言した型の構造体リテラルで、項目が抜けていないか（EXP-64）。
//
// Go のコンパイラは、名前つきの構造体リテラル（T{A: 1}）で項目が抜けていても何も言わない。
// 抜けた項目はゼロ値のまま DB まで届く。項目を足したとき、作る場所の直し忘れがこれで起きる。
//
// 宣言のしかた（型の直前に書く）:
//
//	//smlint:structfill
//	type Schedule struct {
//		ID        int64
//		PlannedAt time.Time
//		Note      string //smlint:optional
//	}
//
// 見ているもの:
//   - 宣言した型の、名前つき構造体リテラル（別パッケージからは公開項目だけ）
//   - 空のリテラル T{}（ただし `return T{}, err` のようにエラーと一緒に返すものは除く）
//
// 見ていないもの:
//   - 名前なしリテラル（T{1, t}）。項目数が合わないとコンパイラが止めるので要らない
//   - 作った後の代入（s.X = ...）・オプション関数の中身。リテラルではないため
//   - 宣言していない型。★既定で全部の型を見ると、ほとんどが「わざと一部だけ埋める」書き方で
//     指摘が埋もれる（このリポジトリで実測: docs/struct-fill.md）。-structfill.all で全部見られる
//
// 逃げ道は //smlint:allow structfill 理由: ... 。
var StructFill = &analysis.Analyzer{
	Name: "structfill",
	Doc: "//smlint:structfill を付けた型の構造体リテラルで、項目が抜けていないかを見る。" +
		"項目を足したときの作る場所の直し忘れ（ゼロ値のまま保存される）を止める（EXP-64）。",
	Run:       runStructFill,
	FactTypes: []analysis.Fact{new(structFillFact)},
}

// StructFillDirective は型に付ける宣言。OptionalDirective は「埋めなくてよい」項目に付ける。
//
// ★どちらも `//名前:語` の形にする（gofmt がディレクティブとして扱う形。AllowPrefix の注意と同じ）。
const (
	StructFillDirective = "smlint:structfill"
	OptionalDirective   = "smlint:optional"
)

// structFillAll は宣言の無い型も含めて全部見るか（騒がしさを測るための設定）。
var structFillAll bool

func init() {
	StructFill.Flags.BoolVar(&structFillAll, "all", false,
		"宣言の無い型も含め、名前つきの構造体リテラルを全部見る（騒がしさの測定用）")
}

// structFillFact は「この型は全項目を埋める」という印。別パッケージの検査へ渡すため Fact にする。
type structFillFact struct {
	Optional []string // 埋めなくてよい項目
}

func (*structFillFact) AFact()           {}
func (f *structFillFact) String() string { return "structfill" }

func runStructFill(pass *analysis.Pass) (any, error) {
	// 1) このパッケージで宣言された型に印を付ける
	for _, f := range pass.Files {
		for _, decl := range f.Decls {
			gd, ok := decl.(*ast.GenDecl)
			if !ok {
				continue
			}
			for _, spec := range gd.Specs {
				ts, ok := spec.(*ast.TypeSpec)
				if !ok {
					continue
				}
				st, ok := ts.Type.(*ast.StructType)
				if !ok {
					continue
				}
				doc := ts.Doc
				if doc == nil && len(gd.Specs) == 1 {
					doc = gd.Doc
				}
				if !hasDirective(doc, StructFillDirective) {
					continue
				}
				obj := pass.TypesInfo.Defs[ts.Name]
				if obj == nil {
					continue
				}
				pass.ExportObjectFact(obj, &structFillFact{Optional: optionalFields(st)})
			}
		}
	}

	// 2) エラーと一緒に返す空のリテラル（return T{}, err）は対象外にする
	errReturn := map[*ast.CompositeLit]bool{}
	for _, f := range pass.Files {
		ast.Inspect(f, func(n ast.Node) bool {
			rs, ok := n.(*ast.ReturnStmt)
			if !ok || len(rs.Results) < 2 {
				return true
			}
			if id, ok := rs.Results[len(rs.Results)-1].(*ast.Ident); ok && id.Name == "nil" {
				return true
			}
			for _, r := range rs.Results[:len(rs.Results)-1] {
				if u, ok := r.(*ast.UnaryExpr); ok {
					r = u.X
				}
				if cl, ok := r.(*ast.CompositeLit); ok {
					errReturn[cl] = true
				}
			}
			return true
		})
	}

	// 3) リテラルを見る
	for _, f := range pass.Files {
		ast.Inspect(f, func(n ast.Node) bool {
			lit, ok := n.(*ast.CompositeLit)
			if !ok {
				return true
			}
			named, st := namedStruct(pass.TypesInfo.TypeOf(lit))
			if named == nil {
				return true
			}
			var optional []string
			if !structFillAll {
				var fact structFillFact
				if !pass.ImportObjectFact(named.Obj(), &fact) {
					return true
				}
				optional = fact.Optional
			}
			if len(lit.Elts) > 0 {
				if _, keyed := lit.Elts[0].(*ast.KeyValueExpr); !keyed {
					return true // 名前なしはコンパイラが全項目を要求する
				}
			} else if errReturn[lit] {
				return true
			}

			set := map[string]bool{}
			for _, e := range lit.Elts {
				if kv, ok := e.(*ast.KeyValueExpr); ok {
					if id, ok := kv.Key.(*ast.Ident); ok {
						set[id.Name] = true
					}
				}
			}
			samePkg := named.Obj().Pkg() == pass.Pkg
			var missing []string
			for i := 0; i < st.NumFields(); i++ {
				fld := st.Field(i)
				if !fld.Exported() && !samePkg {
					continue // 別パッケージからは書けない
				}
				if set[fld.Name()] || contains(optional, fld.Name()) || fld.Name() == "_" {
					continue
				}
				missing = append(missing, fld.Name())
			}
			if len(missing) == 0 {
				return true
			}
			sort.Strings(missing)
			report(pass, "structfill", lit.Pos(),
				"%s の項目が埋まっていない: %s（ゼロ値のまま使われる。埋めるか、項目に //%s を付ける）",
				named.Obj().Name(), strings.Join(missing, ", "), OptionalDirective)
			return true
		})
	}
	return nil, nil
}

// namedStruct は、リテラルの型が名前つきの構造体ならそれを返す（ポインタ・別名は外す）。
func namedStruct(t types.Type) (*types.Named, *types.Struct) {
	if t == nil {
		return nil, nil
	}
	t = types.Unalias(t)
	if p, ok := t.(*types.Pointer); ok {
		t = types.Unalias(p.Elem())
	}
	named, ok := t.(*types.Named)
	if !ok {
		return nil, nil
	}
	st, ok := named.Underlying().(*types.Struct)
	if !ok {
		return nil, nil
	}
	return named, st
}

func optionalFields(st *ast.StructType) []string {
	var out []string
	for _, fld := range st.Fields.List {
		if !hasDirective(fld.Doc, OptionalDirective) && !hasDirective(fld.Comment, OptionalDirective) {
			continue
		}
		for _, n := range fld.Names {
			out = append(out, n.Name)
		}
		if len(fld.Names) == 0 { // 埋め込み
			out = append(out, embeddedName(fld.Type))
		}
	}
	return out
}

func embeddedName(e ast.Expr) string {
	switch x := e.(type) {
	case *ast.StarExpr:
		return embeddedName(x.X)
	case *ast.SelectorExpr:
		return x.Sel.Name
	case *ast.Ident:
		return x.Name
	}
	return ""
}

func hasDirective(cg *ast.CommentGroup, directive string) bool {
	if cg == nil {
		return false
	}
	for _, c := range cg.List {
		text := strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(c.Text), "//"))
		if text == directive || strings.HasPrefix(text, directive+" ") {
			return true
		}
	}
	return false
}

func contains(xs []string, s string) bool {
	for _, x := range xs {
		if x == s {
			return true
		}
	}
	return false
}
