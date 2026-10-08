# EXP-9 保守性の自動検査 — 何を機械に任せ、何を任せないか

再現:

```bash
go build -o /tmp/sqllint ./cmd/sqllint && /tmp/sqllint ./...
go test ./internal/lint/ -v          # 検査そのものの検査（analysistest）
```

結果ファイル: [`docs/results/exp-9/exp-9-static-analysis.md`](results/exp-9/exp-9-static-analysis.md)

---

## 1. 実装した検査（6つ）

これまでの実験で実際に事故になったものだけを対象にした。

| 検査 | 見ているもの | 根拠 |
| --- | --- | --- |
| `rawdb` | domain/service/handler へ生の `*sql.DB` を渡す（引数・構造体フィールド） | [repository-layer.md](repository-layer.md) 2.1 |
| `txhttp` | `Tx`/`WithTx`/`Transaction` に渡した関数の中の HTTP 呼び出し | [locking.md](locking.md) 5、`調査` §4.6 |
| `rowsaffected` | `UPDATE`/`DELETE` の `Exec` の戻り値を捨てている | [repository-layer.md](repository-layer.md) 2.2 |
| `nocontext` | `Exec`/`Query`/`QueryRow`/`Begin`/`Prepare`（Context なし） | [shutdown.md](shutdown.md) |
| `loopquery` | ループの中の1件ずつの問い合わせ（N+1） | [query-plan-skew.md](query-plan-skew.md) 4 |
| `domaintime` | 業務ロジックでの `time.Now()` | [fencing.md](fencing.md) 2.2 |
| `structfill` | `//smlint:structfill` を付けた型の構造体リテラルの項目漏れ（EXP-64 で追加） | [struct-fill.md](struct-fill.md) |

各検査の Doc に「**何を見ていて、何を見ていないか**」を書いてある。

## 2. 実装しないと決めたもの（理由つき）

指示書にあった項目のうち、次は**構文だけでは判定できない**ので実装していない。
できないものを「できる」と言わないことが、この検査を信用に足るものにする。

| 項目 | 実装しない理由 | 代わりにどうするか |
| --- | --- | --- |
| unmanaged goroutine | 「管理されている」は構文で判定できない。`WaitGroup` があっても待っているとは限らない | 終了時の goroutine 数をテストで縛る（[shutdown.md](shutdown.md) 3） |
| unbounded channel | バッファ無しチャネルは「無制限」ではない。危険なのはスライスを溜め続けるコードで、これは構文では見分けられない | 負荷をかけて測る（[backpressure.md](backpressure.md)） |
| migration file の変更 | 静的解析ではなく checksum で検出できる | 起動時に止める（[migration-crash.md](migration-crash.md) 4） |
| scope 無し repository 呼び出し | 型で禁止できている（`*repo.Scope` しか受け取らない） | 検査より型のほうが確実 |

## 3. このリポジトリに当てた結果

| 段階 | 指摘 |
| --- | --- |
| 作った直後（全ファイル） | **191 件** |
| `_test.go` を対象外にした | 43 件 |
| `loopquery` の誤検出を直した | 27 件 |
| 対応後（修正 + 理由つきの逃げ道） | **0 件** |

### 3.1 テストコードを外した理由

191 件のうち **148 件がテストコード**だった。後片付けの `DELETE` で影響行数を見ない、
といった書き方が大半で、本番コードの問題が埋もれる。既定で `_test.go` は対象外にした
（`lint.IncludeTests` で戻せる）。

### 3.2 `loopquery` の誤検出を直した

43 件のうち 16 件が **「ループ変数が SQL 文そのもの」** という形だった。

```go
for _, stmt := range statements {   // マイグレーションや後片付け
    db.ExecContext(ctx, stmt)       // ← 行を1件ずつ引いているのではない
}
```

**ループ変数が束縛値として使われている場合だけ**指摘するように直した。

```go
for _, id := range ids {
    db.QueryRowContext(ctx, q, id)  // ← これは N+1
}
```

### 3.3 ★本物が1件見つかった

27 件の中に、実際のバグが 1 件あった。

```go
// repo.Migrate の完了記録（修正前）
d.sqldb.ExecContext(ctx, "UPDATE schema_migrations SET ... state='done' WHERE version = ?", v)
// 影響行数を見ていない → 記録できていないのに「完了」と思い込む
```

記録が 0 行だった場合、次の起動は「適用済み」と判断して黙って先へ進む。
EXP-6 で作った安全網（started → done の二相記録）が、ここで無効になる。
**影響行数を確かめるように修正した。**

### 3.4 残りは逃げ道（理由つき）

残る指摘は実験コードそのもの（＝測定対象）だったので、理由つきの逃げ道を入れた。

```go
//smlint:allow loopquery 理由: EXP-7 の実験対象。N+1 の遅さを測るための実装
```

- **理由が無い逃げ道は認めない**（検査が「理由を書け」と指摘する）
- 逃げ道は `grep -rn "smlint:allow"` で一覧でき、数も数えられる
- いま 0 件なので、**新しい違反はそのまま浮かび上がる**

## 4. gofmt に消された話

最初は `//sample-manual:allow ...` という書き方にしていたが、
**gofmt が `// sample-manual:allow ...` と空白を入れて整形**してしまい、逃げ道が効かなくなった。

gofmt がディレクティブとして扱うのは `//名前:語` の形で、名前にハイフンを含められない。
`//smlint:allow` に変えて解決した（テストが落ちて気づいた）。

## 4.1 検査自身のバグ（あとから出た）

EXP-10 のコードを足したところ、リポジトリ全体に当てた瞬間に落ちた。

```
fatal error: concurrent map writes
	github.com/aq35/sample_manual/internal/lint.allowed(...)
```

逃げ道の使用回数を数える map にロックが無かった。
`go/analysis` は**パッケージごとに並列で**検査を走らせるので、ここは同時に書かれる。

**逃げ道が少ないうちは書き込みが稀で、たまたま落ちていなかった。**
「このリポジトリで 0 件だった」は「正しく動く」ことの証明ではない、という実例。
`sync.Mutex` を足して直した（`internal/lint/lint.go`）。

## 5. 適用範囲と未検証

**当てはまる範囲**

- Go の構文と型情報だけを見る（`go/analysis`）。実行時の振る舞いは見ていない
- 「domain 層」の判定はパッケージパスの文字列（`/domain` `/service` `/usecase` `/handler` `/http` `/api`）

**保証しないこと**

- interface に包んで渡された `*sql.DB` は検出できない（`rawdb`）
- 別関数に切り出された HTTP 呼び出しは検出できない（`txhttp`）。関数をまたぐ追跡はしていない
- 動的に組み立てた SQL は判定できない（`rowsaffected`）
- 呼び出し先の中にあるクエリは検出できない（`loopquery`）
- **誤検出率は、このリポジトリでしか測っていない。** 他のプロジェクトに当てたときの値は未測定
