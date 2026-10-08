# 構造体のセット漏れを機械で止める（EXP-64）

構造体に項目を1つ足したとき、**作る場所の直し忘れ**は Go のコンパイラでは止まらない。
名前つきのリテラル（`T{A: 1}`）は、書かなかった項目をゼロ値で埋めて黙って通る。
ゼロ値はそのまま DB に保存され、テストも通る（テストデータにも足し忘れているので）。

実装は [internal/structfilllab](../internal/structfilllab) と
[internal/lint/structfill.go](../internal/lint/structfill.go)、receipt は
[docs/results/exp-64](results/exp-64/exp-64-struct-fill.md)。

```bash
go test ./internal/structfilllab/ -run TestEXP64 -v   # MySQL は要らない
go run ./cmd/sqllint -structfill ./...                # 宣言した型の項目漏れを見る
```

## 1. 結果: 作り方4種 × 止める仕組み

作る場所 10 箇所。`Schedule` に `PlannedQty int` を足し、**呼ぶ側は直さない**。

| 作り方 | コンパイラだけ | ＋structfill 検査 | 備考 |
| --- | --- | --- | --- |
| 直接リテラル `model.Schedule{ID: 1, PlannedAt: at}` | **10/10 素通り** | 0/10 | 検査が 10 箇所すべてを指摘 |
| オプション関数 `NewSchedule(id, at, opts...)` | **10/10 素通り** | **10/10 素通り** | 検査はコンストラクタの中の 1 箇所だけ指摘。呼ぶ側は関数呼び出しなので見えない |
| 引数 `NewSchedule(id, at, qty)` | 0/10（**10 件コンパイルエラー**） | 0/10 | コンパイラだけで止まる唯一の形 |
| 引数用 struct `NewSchedule(ScheduleParams{...})` | **10/10 素通り** | 0/10 | Params に宣言を付ければ検査が 10 箇所を指摘 |

### 変換関数（DB の行 ↔ 業務の型）の写し忘れ

| 壊れ方 | 手書きのテストデータ（ID・TenantID・PlannedAt だけ） | 全項目を自動で埋めたデータ（`FillNonZero`） |
| --- | --- | --- |
| `PlannedQty` の写し忘れ | **通ってしまう**（ゼロ値どうしで一致） | 落ちる（差: PlannedQty） |
| `PlannedAt` と `ActualAt` の取り違え | **通ってしまう**（ActualAt が nil） | 落ちる（差: PlannedAt） |

手書きのテストデータは「書いた項目」しか確かめない。項目を足すと、テストデータにも足し忘れる。
**全項目を自動で埋めれば、新しい項目は何もしなくても確かめる対象に入る。**
値を項目ごとに変えているので、同じ型の項目の取り違えも見つかる。

### 全部の型を見ると騒がしすぎる

このリポジトリに、宣言なしで全部の型を見る設定（`-structfill.all`）を当てると
**165 件（71 型）**。ほとんどが「わざと一部だけ埋める」書き方（設定の構造体、テストの部品など）で、
本当に守りたい型の指摘が埋もれる。だから既定は**宣言した型だけ**にした
（[exhaustruct](https://github.com/GaijinEntertainment/go-exhaustruct) でも `-explicit`＋`//exhaustruct:enforce` で同じ形にできる）。

## 2. 判断の表

| 状況 | どうするか |
| --- | --- |
| 必ず入れる項目（空のまま保存されたら困る） | **コンストラクタの引数にする**。足すと呼ぶ側すべてがコンパイルエラーになる |
| 入れなくてもいい項目（決まった初期値がある） | オプション関数でよい。**必須の項目をオプション関数で渡さない** |
| 必須の項目が多い（6 個以上で順番を取り違えやすい） | 引数用 struct ＋ `//smlint:structfill`。名前で書けて、漏れは検査が止める |
| 型の外から直接リテラルで作られてしまう | 守りたい型に `//smlint:structfill`（または exhaustruct）を付けて CI で止める |
| 変換関数（行 ↔ 型、型 ↔ API） | 作る場所をここ 1 つに寄せ、`FillNonZero` でラウンドトリップテストを 1 本書く |
| 同じ型の項目の取り違え（予定日時 ↔ 実績日時） | 型を分ける（`Plan` / `Actual`）。それまでは項目ごとに違う値で埋めたテストで止める |
| 任意の項目 | 項目に `//smlint:optional` を付ける（「書かなくてよい」を宣言として残す） |

## 3. 導入の順番（既存のコードに入れるとき）

1. **作る場所を数える**: `go run ./cmd/sqllint -structfill -structfill.all ./... | grep 型名`
2. **作る場所を変換関数・コンストラクタに寄せる**（場所が減るほど漏れが減る）
3. **必須の項目を引数へ**移す（オプション関数で渡していた必須項目を含む）
4. 守りたい型に **`//smlint:structfill`** を付け、CI の sqllint で止める
5. 変換関数に **`FillNonZero` のラウンドトリップテスト**を 1 本ずつ足す
6. 余裕があれば、行の型や変換をコード生成に任せる（sqlc＝スキーマから型、goverter＝写していない項目を生成時にエラー）

## 4. 見ていないもの（限界）

- **作った後の代入**（`s.PlannedQty = q`）・オプション関数の中身。リテラルではないので検査は見ない
- **同じ型の引数の順番違い**（`NewSchedule(id, qty, ...)` の取り違え）。コンパイラも検査も止めない
- **ORM が反射で埋める型**（GORM などで map や部分更新を使う場合）
- 数字は合成コード（作る場所 10 箇所）で数えた。実際の素通りの数は、作り方の混ざり具合で変わる

## 5. 持って行けるもの

| もの | 使い方 |
| --- | --- |
| `internal/structfilllab/fill.go` の `FillNonZero` / `DiffFields` | reflect だけ。ラウンドトリップテストにそのまま使える |
| `internal/lint/structfill.go` | go/analysis の検査。`multichecker` に足すか、exhaustruct で代用する |

### ラウンドトリップテストの形

```go
func TestScheduleRoundTrip(t *testing.T) {
	var want Schedule
	if err := structfilllab.FillNonZero(&want); err != nil { // 全項目をゼロでない別々の値に
		t.Fatal(err)
	}
	got := FromRow(ToRow(want))
	if d := structfilllab.DiffFields(want, got); len(d) > 0 {
		t.Fatalf("写し忘れ・取り違え: %v", d)
	}
}
```

## 参考

- [GaijinEntertainment/go-exhaustruct](https://github.com/GaijinEntertainment/go-exhaustruct) — 全項目の初期化を検査する linter。v5 は `-explicit`・`//exhaustruct:enforce`・`//exhaustruct:optional`。
  `return T{}, err` の空リテラルは既定で許す（structfill も同じ扱いにした）
- [go.abhg.dev/requiredfield](https://beta.pkg.go.dev/go.abhg.dev/requiredfield) — 項目に `// required` を付けた分だけ検査する
- Uber Go Style Guide の Functional Options — 必須の値は普通の引数（`Open(addr string, opts ...Option)`）、任意の設定だけをオプションに
