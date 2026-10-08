# EXP-64 項目を足したときの『作る場所の直し忘れ』を、コンパイラ・検査・テストのどれが止めるか

| | |
| --- | --- |
| Experiment | EXP-64 / struct-fill |
| Starting SHA | `5daa39a61f32` |
| Meter version | `expkit/2` |
| Hypothesis (frozen before result) | 1) 直接リテラル・オプション関数・引数用 struct は、項目を足してもコンパイルが通る（直し忘れ 10/10 が素通り）。 2) 必須の値を引数にした形だけは、呼ぶ側 10/10 がコンパイルエラーになる。 3) structfill 検査（宣言した型だけ見る）は、直接リテラルと引数用 struct の 10/10 を止める。    オプション関数は呼ぶ側を止められない（作る場所が関数呼び出しで、リテラルではない）。 4) 変換関数の写し忘れは、手書きのテストデータでは通り、全項目を自動で埋めたラウンドトリップで落ちる。 5) 宣言の無い型まで全部見ると、このリポジトリでも指摘が多すぎて使えない（だから宣言した型だけにする）。 |
| Environment | go1.26.0 linux/amd64 cpu=4 gomaxprocs=4 mysql= sha=5daa39a61f32 |
| Started / Ended | 2026-10-08T23:22:37Z / 2026-10-08T23:22:54Z |

## Workload

- `change` = Schedule に PlannedQty int を足す。呼ぶ側は直さない
- `sites` = 10

## Results

### 作り方: literal — OK

呼ぶ側が model.Schedule{ID: 1, PlannedAt: at} と直接書く

| 数えたもの | 値 |
| --- | --- |
| compile_errors | 0 |
| lint_findings_callers | 10 |
| lint_findings_model | 0 |
| silent_compile_only | 10 |
| silent_compile_plus_lint | 0 |
| sites | 10 |

- コンパイラだけ: 10/10 が素通り。コンパイラ＋structfill: 0/10 が素通り（model 側の指摘 0）

### 作り方: options — **事故あり**

NewSchedule(id, at, opts...)。足した項目は WithPlannedQty で渡す（渡し忘れても通る）

| 数えたもの | 値 |
| --- | --- |
| compile_errors | 0 |
| lint_findings_callers | 0 |
| lint_findings_model | 1 |
| silent_compile_only | 10 |
| silent_compile_plus_lint | 10 |
| sites | 10 |

- コンパイラだけ: 10/10 が素通り。コンパイラ＋structfill: 10/10 が素通り（model 側の指摘 1）

### 作り方: positional — OK

NewSchedule(id, at, qty)。足した項目は引数に足す

| 数えたもの | 値 |
| --- | --- |
| compile_errors | 10 |
| lint_findings_callers | 0 |
| lint_findings_model | 0 |
| silent_compile_only | 0 |
| silent_compile_plus_lint | 0 |
| sites | 10 |

- コンパイラだけ: 0/10 が素通り。コンパイラ＋structfill: 0/10 が素通り（model 側の指摘 0）

### 作り方: params — OK

NewSchedule(ScheduleParams{...})。足した項目は Params に足す

| 数えたもの | 値 |
| --- | --- |
| compile_errors | 0 |
| lint_findings_callers | 10 |
| lint_findings_model | 0 |
| silent_compile_only | 10 |
| silent_compile_plus_lint | 0 |
| sites | 10 |

- コンパイラだけ: 10/10 が素通り。コンパイラ＋structfill: 0/10 が素通り（model 側の指摘 0）

### ラウンドトリップ: 正しい変換 — OK

- 手書きのテストデータ（ID・TenantID・PlannedAt だけ）: 差 [] → 通る（正しい）
- FillNonZero で全項目を埋めたデータ: 差 [] → 通る（正しい）

### ラウンドトリップ: PlannedQty の写し忘れ — OK

- 手書きのテストデータ（ID・TenantID・PlannedAt だけ）: 差 [] → ★通ってしまう（見逃し）
- FillNonZero で全項目を埋めたデータ: 差 [PlannedQty] → 落ちる（検出）

### ラウンドトリップ: PlannedAt と ActualAt の取り違え — OK

- 手書きのテストデータ（ID・TenantID・PlannedAt だけ）: 差 [] → ★通ってしまう（見逃し）
- FillNonZero で全項目を埋めたデータ: 差 [PlannedAt] → 落ちる（検出）

### ラウンドトリップのまとめ — OK

| 数えたもの | 値 |
| --- | --- |
| broken_mappers | 2 |
| missed_by_fillnonzero | 0 |
| missed_by_hand_data | 2 |

### このリポジトリ: 宣言なしで全部の型を見た（-structfill.all） — **事故あり**

| 数えたもの | 値 |
| --- | --- |
| distinct_types | 71 |
| findings | 165 |

- 指摘の多い型: FieldContext=25, Op=22, Analyzer=8, LatencyStats=7, Record=6, Row=4, Lever=4, Options=4

### このリポジトリ: 宣言した型だけを見た（既定） — OK

| 数えたもの | 値 |
| --- | --- |
| findings | 0 |

- 宣言しているのは internal/structfilllab の Schedule / ScheduleRow だけ。事故側の再現コードは理由つきの逃げ道

## Verdict

項目を足しても、直接リテラル・オプション関数・引数用 struct はコンパイルが通り、作る場所の直し忘れは素通りする。コンパイラが止めるのは『必須の値を引数にした形』だけ。リテラルは structfill 検査（宣言した型だけ）で止まり、変換関数の写し忘れは全項目を自動で埋めたラウンドトリップテストで止まる（手書きのテストデータでは通る）。オプション関数は任意の項目専用にする。全部の型を見ると騒がしすぎるので、守りたい型にだけ宣言を付ける。

## 適用範囲

- Go の構造体リテラルとコンストラクタ。ORM が反射で埋める型（GORM の Create など）は対象外
- 検査は『リテラルで作る場所』だけを見る。作った後の代入（s.X = ...）は見ない

## 保証しない範囲・未検証

- 合成コードで数えた。実際のコードは作り方が混ざるので、素通りの数は混ざり具合で変わる
- 引数が多い（6個以上）ときの順番の取り違えは、型が同じだとコンパイラも検査も止めない。Plan/Actual のように型を分けるか、引数用 struct＋検査にする

## 再利用できる成果物

- internal/lint/structfill.go: //smlint:structfill を付けた型の項目漏れを見る検査（sqllint に同梱）
- internal/structfilllab/fill.go: FillNonZero / DiffFields（reflect だけ。他プロジェクトへそのまま持って行ける）
- docs/struct-fill.md: 判断の表と導入の順番

