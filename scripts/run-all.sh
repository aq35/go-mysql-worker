#!/usr/bin/env bash
# 資料の主張を、ぜんぶ実行して確かめる。
#
#   ./scripts/run-all.sh              # 実行ログは .run-logs/ へ（git 管理外）。receipt は書かない
#   EXP_RECORD=1 ./scripts/run-all.sh # 実験の receipt を docs/results/ に固定名で保存する
#
# ★通常実行は repo を汚さない。生ログは .run-logs/（.gitignore 済）へ出す。
# 実験の receipt（docs/results/<unit>/）を更新するのは EXP_RECORD=1 のときだけ。
# これが無いと full suite のたびに result が書き換わり、作業ツリーが dirty になる
# （実際にそれで doc のリンクが毎回ずれた。scripts/postflight.sh の worktree gate が捕まえる）。
#
# MYSQL_DSN が未設定なら、DB を使う項目は skip される（Go 単体の項目は動く）。
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=${RUN_LOG_DIR:-.run-logs}
mkdir -p "$OUT"
stamp=$(date +%Y%m%d-%H%M%S)

# 実験の前に MySQL の baseline を取る（postflight で使う）。
BASELINE=""
if [[ -n "${MYSQL_DSN:-}" ]]; then
  BASELINE=$(scripts/postflight.sh baseline 2>/dev/null || echo "")
fi

run() { # run <出力ファイル> <説明> <コマンド...>
  local file="$1" desc="$2"; shift 2
  echo "==> ${desc}"
  {
    echo "# ${desc}"
    echo "# 実行: $(date -Is)"
    echo "# コマンド: $*"
    echo
    "$@" 2>&1
  } | tee "${OUT}/${file}"
  echo
}

echo "Go: $(go version)"
if [[ -n "${MYSQL_DSN:-}" ]]; then
  echo "MYSQL_DSN: 設定あり"
else
  echo "MYSQL_DSN: 未設定（DB を使う項目は skip されます）"
fi
echo

run "01-tests-${stamp}.txt"      "① 設計チェックリストのテスト（§2〜§6）" \
    go test ./... -v -timeout 10m
run "02-json-bench-${stamp}.txt" "② JSON の扱い（§6.1 / §6.2）" \
    go test ./internal/jsonx/ -bench . -benchmem -run XXX -count=3
run "03-compare-bench-${stamp}.txt" "③ 状態比較のコスト（§8）" \
    go test ./internal/model/ -bench Compare -benchmem -run XXX -count=3
run "04-memory-${stamp}.txt"     "④ 1対象あたりのメモリ（§3.3）" \
    go test ./internal/worker/ -run Memory -v

if [[ -n "${MYSQL_DSN:-}" ]]; then
  run "05-mysql-facts-${stamp}.txt" "⑤ MySQL の挙動確認（§9 の未検証項目）" \
      go test ./internal/mysqlfacts/ -v -timeout 10m
  run "06-write-bench-${stamp}.txt" "⑥ 書き込み方法の比較（§4.3）" \
      go test ./internal/store/ -bench Write -benchtime 30x -run XXX -count=3
  run "07-loadsim-${stamp}.txt"     "⑦ 負荷シミュレーション（§4.1 の表）" \
      go run ./cmd/loadsim -rate 1000 -duration 10s -robots 1000 -change-rate 0.01
  run "08-repo-experiments-${stamp}.txt" "⑧ リポジトリ層の実験（表設計・読み方・事故）" \
      go test ./internal/repo/ -run TestExperiment_ -v -timeout 20m
  run "09-repo-guards-${stamp}.txt" "⑨ リポジトリ層が事故を止めることの確認" \
      go test ./internal/repo/... -run 'TestScope|TestRepo|TestTx|TestPage|TestCheck|TestBind|TestExpect|TestHarden' -v -timeout 10m
  run "10-repo-bench-${stamp}.txt" "⑩ リポジトリ層の代金" \
      go test ./internal/repo/ -bench 'PointRead|CheckAndBind' -benchmem -run XXX -count=3
  run "11-locking-${stamp}.txt" "⑪ 排他制御の実験（GET_LOCK の性質と代替）" \
      go test ./internal/repo/ -run 'TestExperimentLock_|TestWithLock|TestMigrate' -v -timeout 10m

  # ---- 異常系の実験（EXP-1..3, 6）。結果は docs/results/exp-N/ にも保存される ----
  run "12-exp1-effect-crash-${stamp}.txt" "⑫ EXP-1 外部 effect 途中の SIGKILL" \
      go test ./internal/effectlab/ -run TestEXP1 -v -timeout 20m
  run "13-exp2-fencing-${stamp}.txt" "⑬ EXP-2 lease / fencing / 時計のずれ" \
      go test ./internal/fencelab/ -run TestEXP2 -v -timeout 20m
  run "14-exp3-shutdown-${stamp}.txt" "⑭ EXP-3 graceful shutdown" \
      go test ./internal/shutdownlab/ -run TestEXP3 -v -timeout 20m
  run "15-exp6-migration-crash-${stamp}.txt" "⑮ EXP-6 マイグレーション途中の crash" \
      go test ./internal/repo/ -run TestEXP6 -v -timeout 20m
  run "16-exp4-backpressure-${stamp}.txt" "⑯ EXP-4 backpressure と過負荷" \
      go test ./internal/loadlab/ -run TestEXP4 -v -timeout 20m
  run "17-exp5-pool-${stamp}.txt" "⑰ EXP-5 接続プールの飽和点" \
      go test ./internal/poollab/ -run TestEXP5 -v -timeout 20m
  run "18-exp7-plan-${stamp}.txt" "⑱ EXP-7 実行計画とデータの偏り" \
      go test ./internal/planlab/ -run TestEXP7 -v -timeout 20m
  run "22-exp11-backup-${stamp}.txt" "㉒ EXP-11 バックアップ・復元・破損" \
      go test ./internal/backuplab/ -run TestEXP11 -v -timeout 20m
  run "23-exp12-cadence-${stamp}.txt" "㉓ EXP-12 ワーカーのポーリング頻度" \
      go test ./internal/cadencelab/ -run TestEXP12 -v -timeout 20m
  run "24-exp13-credrotate-${stamp}.txt" "㉔ EXP-13 DB資格情報のローテーション" \
      go test ./internal/credlab/ -run TestEXP13 -v -timeout 20m
  run "25-exp14-fanout-${stamp}.txt" "㉕ EXP-14 fan-out を畳む" \
      go test ./internal/fanoutlab/ -run TestEXP14 -v -timeout 20m
  run "26-exp15-contention-${stamp}.txt" "㉖ EXP-15 テーブル分割と競合" \
      go test ./internal/contentionlab/ -run TestEXP15 -v -timeout 20m
  run "27-exp16-inlimit-${stamp}.txt" "㉗ EXP-16 担当テナント数の上限" \
      go test ./internal/fanoutlab/ -run TestEXP16 -v -timeout 20m
  run "28-exp17-backoff-${stamp}.txt" "㉘ EXP-17 適応的バックオフ" \
      go test ./internal/cadencelab/ -run TestEXP17 -v -timeout 20m
  run "29-tenantworker-${stamp}.txt" "㉙ 統合ワーカー（lease×fanout×backoff）" \
      go test ./internal/tenantworker/ -v -timeout 10m
  run "30-kascontract-${stamp}.txt" "㉚ KAS 契約（両engineで同一domain結果）" \
      go test ./internal/kascontract/ -run TestContract -v -timeout 10m
  run "32-exp18-datesearch-${stamp}.txt" "㉜ EXP-18 日付範囲検索は何件で重くなるか" \
      go test ./internal/datelab/ -run TestEXP18 -v -timeout 20m
  run "33-exp19-columnsplit-${stamp}.txt" "㉝ EXP-19 メモ列の縦分割（同居 vs 別表）" \
      go test ./internal/splitlab/ -run TestEXP19 -v -timeout 20m
  run "34-exp20-readreplica-${stamp}.txt" "㉞ EXP-20 予定/実績を primary とレプリカで読み分ける" \
      go test ./internal/readrouter/ -run TestEXP20 -v -timeout 20m
  run "35-costgate-${stamp}.txt" "㉟ クエリコストゲート（走査見込みで実行前に弾く）" \
      go test ./internal/repo/ -run TestCostGate -v -timeout 20m
  run "36-exp21-width-${stamp}.txt" "㊱ EXP-21 VARCHAR/TEXT の重さ（inline/off-page）" \
      go test ./internal/widthlab/ -run TestEXP21 -v -timeout 20m
  run "37-exp22-status-${stamp}.txt" "㊲ EXP-22 ステータスでテーブルを分けるべきか" \
      go test ./internal/statuslab/ -run TestEXP22 -v -timeout 20m
  run "38-exp23-gqlperf-${stamp}.txt" "㊳ EXP-23 gqlgen パフォーマンス（N+1/DataLoader）" \
      go test ./internal/gqllab/ -run TestEXP23 -v -timeout 20m
  run "39-exp24-gqlsec-${stamp}.txt" "㊴ EXP-24 gqlgen セキュリティ（テナント分離・複雑度）" \
      go test ./internal/gqllab/ -run TestEXP24 -v -timeout 20m
  run "40-exp25-gqlauthz-${stamp}.txt" "㊵ EXP-25 gqlgen 認可（@auth・フィールド単位ロール）" \
      go test ./internal/gqllab/ -run TestEXP25 -v -timeout 20m
  run "41-exp26-gqladmit-${stamp}.txt" "㊶ EXP-26 gqlgen 受付制御（allowlist・レート制限）" \
      go test ./internal/gqllab/ -run TestEXP26 -v -timeout 20m
  run "42-exp27-gqlmut-${stamp}.txt" "㊷ EXP-27 gqlgen mutation 冪等性・入力検証" \
      go test ./internal/gqllab/ -run TestEXP27 -v -timeout 20m
  run "43-exp28-gqlrowauthz-${stamp}.txt" "㊸ EXP-28 gqlgen 行レベル認可" \
      go test ./internal/gqllab/ -run TestEXP28 -v -timeout 20m
  run "44-exp29-procsep-${stamp}.txt" "㊹ EXP-29 Web/Worker のプール分離" \
      go test ./internal/procseplab/ -run TestEXP29 -v -timeout 20m
  run "45-exp30-redundancy-${stamp}.txt" "㊺ EXP-30 冗長化とアプリ構造" \
      go test ./internal/redundancylab/ -run TestEXP30 -v -timeout 20m
  run "46-exp31-capacity-${stamp}.txt" "㊻ EXP-31 1タスクの容量（SSE・Worker）" \
      go test ./internal/capacitylab/ -run TestEXP31 -v -timeout 20m
  run "47-exp32-migration-${stamp}.txt" "㊼ EXP-32 無停止スキーマ変更（expand/contract）" \
      go test ./internal/deploylab/ -run TestEXP32 -v -timeout 20m
  run "48-exp33-resilience-${stamp}.txt" "㊽ EXP-33 DB 切断への耐性" \
      go test ./internal/resiliencelab/ -run TestEXP33 -v -timeout 20m
  run "49-exp34-fairness-${stamp}.txt" "㊾ EXP-34 テナント公平性" \
      go test ./internal/fairnesslab/ -run TestEXP34 -v -timeout 20m
  run "50-exp35-timezone-${stamp}.txt" "㊿ EXP-35 タイムゾーン/DST" \
      go test ./internal/tzlab/ -run TestEXP35 -v -timeout 20m
  run "51-exp36-timeout-${stamp}.txt" "(51) EXP-36 クエリタイムアウト/キャンセル" \
      go test ./internal/cancellab/ -run TestEXP36 -v -timeout 20m
  run "53-exp38-sse-${stamp}.txt" "(53) EXP-38 SSE ファンイン対策・hub 上限" \
      go test ./internal/ssehub/ -run TestEXP38 -v -timeout 20m
  run "54-exp39-sse-endpoint-${stamp}.txt" "(54) EXP-39 動く SSE エンドポイント" \
      go test ./internal/ssehub/ -run TestEXP39 -v -timeout 20m
fi
run "57-exp42-hub-isolation-${stamp}.txt" "(57) EXP-42 hub のテナント分離" \
    go test ./internal/ssehub/ -run TestEXP42 -v -timeout 20m
run "58-exp43-pubsub-${stamp}.txt" "(58) EXP-43 pub/sub 跨ぎの SSE fan-out" \
    go test ./internal/pubsub/ -run TestEXP43 -v -timeout 20m
if [[ -n "${MYSQL_DSN:-}" ]]; then
  run "59-exp44-outbox-${stamp}.txt" "(59) EXP-44 トランザクショナル outbox" \
      go test ./internal/outboxlab/ -run TestEXP44 -v -timeout 20m
  run "60-exp45-deadletter-${stamp}.txt" "(60) EXP-45 poison / dead-letter" \
      go test ./internal/dlqlab/ -run TestEXP45 -v -timeout 20m
  run "61-exp48-retention-${stamp}.txt" "(61) EXP-48 保持期間の運用（パーティション DROP）" \
      go test ./internal/retentionlab/ -run TestEXP48 -v -timeout 20m
  run "62-exp49-retry-${stamp}.txt" "(62) EXP-49 一時 vs 恒久エラーの分類とリトライ" \
      go test ./internal/retrylab/ -run TestEXP49 -v -timeout 20m
  run "67-exp52-hubcache-${stamp}.txt" "(67) EXP-52 hub のキャッシュ破棄の粒度" \
      go test ./internal/hubcachelab/ -run TestEXP52 -v -timeout 20m
  run "69-exp54-pk-${stamp}.txt" "(69) EXP-54 主キー設計（連番 vs UUID）" \
      go test ./internal/pklab/ -run TestEXP54 -v -timeout 20m
  run "70-exp55-isolation-${stamp}.txt" "(70) EXP-55 分離レベル（RR vs RC）" \
      go test ./internal/isolationlab/ -run TestEXP55 -v -timeout 20m
  run "71-exp56-bulk-${stamp}.txt" "(71) EXP-56 bulk INSERT の正規化" \
      go test ./internal/bulklab/ -run TestEXP56 -v -timeout 20m
  run "72-exp57-charset-${stamp}.txt" "(72) EXP-57 utf8mb4 と index 長・照合" \
      go test ./internal/charsetlab/ -run TestEXP57 -v -timeout 20m
  run "73-exp58-scope-${stamp}.txt" "(73) EXP-58 共有ワーカーのテナントスコープ強制" \
      go test ./internal/scopelab/ -run TestEXP58 -v -timeout 20m
  run "75-exp60-budget-${stamp}.txt" "(75) EXP-60 接続予算とオートスケール・ストーム" \
      go test ./internal/budgetlab/ -run TestEXP60 -v -timeout 20m
  run "77-exp62-doorbell-${stamp}.txt" "(77) EXP-62 イベント駆動 worker（doorbell＋floor）" \
      go test ./internal/doorbelllab/ -run TestEXP62 -v -timeout 20m
  run "78-exp63-tenant-assignment-${stamp}.txt" "(78) EXP-63 テナント割り当て lease（均等・失敗時全責任・fence 単調）" \
      go test ./internal/assignlab/ -run TestEXP63 -v -timeout 20m
fi

# ---- MySQL が無くても走る（追加ぶん・可観測性/容量計算/サブスク配線）----
run "52-exp37-observability-${stamp}.txt" "(52) EXP-37 可観測性（カーディナリティ・コスト）" \
    go test ./internal/metrics/ -run TestEXP37 -v -timeout 20m
run "55-exp40-ssecapacity-${stamp}.txt" "(55) EXP-40 SSE 容量計算（hub あり/なし）" \
    go test ./internal/ssecapacity/ -run TestEXP40 -v -timeout 20m
run "56-exp41-gqlsub-${stamp}.txt" "(56) EXP-41 gqlgen サブスクリプション（hub 配線）" \
    go test ./internal/gql/ -run TestEXP41 -v -timeout 20m
run "63-exp46-cache-${stamp}.txt" "(63) EXP-46 キャッシュ無効化（TTL/イベント失効/stampede）" \
    go test ./internal/cachelab/ -run TestEXP46 -v -timeout 20m
run "64-exp47-ordering-${stamp}.txt" "(64) EXP-47 順序・冪等消費（版で単調適用）" \
    go test ./internal/orderlab/ -run TestEXP47 -v -timeout 20m
run "74-exp59-submem-${stamp}.txt" "(74) EXP-59 gqlgen subscription の実メモリ単価" \
    go test ./internal/subcaplab/ -run TestEXP59 -v -timeout 20m
run "76-exp61-cost-${stamp}.txt" "(76) EXP-61 月予算での縦/横/Aurora の優先順位" \
    go test ./internal/costlab/ -run TestEXP61 -v -timeout 20m
run "79-exp64-struct-fill-${stamp}.txt" "(79) EXP-64 構造体のセット漏れ（コンパイラ・検査・ラウンドトリップ）" \
    go test ./internal/structfilllab/ -run TestEXP64 -v -timeout 20m
run "65-exp50-memcost-${stamp}.txt" "(65) EXP-50 型ごとのメモリ単価" \
    go test ./internal/memlab/ -run TestEXP50 -v -timeout 20m
run "66-exp51-loopcost-${stamp}.txt" "(66) EXP-51 ループとアロケーションのコスト" \
    go test ./internal/looplab/ -run TestEXP51 -v -timeout 20m
run "68-exp53-revauth-${stamp}.txt" "(68) EXP-53 長寿命接続の途中失権" \
    go test ./internal/revauthlab/ -run TestEXP53 -v -timeout 20m

# ---- MySQL が無くても走る（追加ぶん）----
run "31-config-${stamp}.txt" "㉛ config / tenantcache / secretcache / poolbudget" \
    go test ./internal/config/ ./internal/tenantcache/ ./internal/secretcache/ ./internal/poolbudget/ ./internal/appx/ ./internal/moio/ -v -timeout 10m

# ---- postflight: 「テストが exit 0」だけを成功条件にしない ----
# tests exit 0 AND MySQL alive AND read/write probe AND 接続が baseline へ戻る
# AND プロセス残存なし AND 想定外スキーマなし AND 作業ツリー clean AND receipt manifest 妥当
POSTFLIGHT_OK=1
if [[ -n "${MYSQL_DSN:-}" && -n "$BASELINE" ]]; then
  echo "==> postflight（健全性ゲート）"
  if ! scripts/postflight.sh check "run-all" $BASELINE; then POSTFLIGHT_OK=0; fi
  # 想定外のデータベースが増えていないか（実験が後始末を忘れていないか）
  extra=$(mysql -uroot -N -e "SHOW DATABASES" 2>/dev/null \
    | grep -vE '^(information_schema|mysql|performance_schema|sys|workerdb|workerdb2|postflight_probe)$' || true)
  if [[ -n "$extra" ]]; then echo "POSTFLIGHT FAIL: 想定外のデータベース: $extra"; POSTFLIGHT_OK=0; fi
fi
# tracked に実行形式が紛れていないか
if ! scripts/check-no-binaries.sh >/dev/null 2>&1; then
  echo "POSTFLIGHT FAIL: tracked に実行形式が含まれている（scripts/check-no-binaries.sh）"; POSTFLIGHT_OK=0
fi
# 作業ツリーが clean か（EXP_RECORD 実行では receipt 更新を除いて判定）
dirty=$(git status --porcelain | grep -vE '^\?\? \.run-logs/' || true)
if [[ -z "${EXP_RECORD:-}" && -n "$dirty" ]]; then
  echo "POSTFLIGHT FAIL: 作業ツリーが dirty（通常実行は repo を書き換えないはず）:"; echo "$dirty" | head
  POSTFLIGHT_OK=0
fi
# receipt manifest（latest.json）が妥当か
for lj in docs/results/exp-*/latest.json; do
  [ -f "$lj" ] || continue
  if ! grep -q '"meter_version"' "$lj"; then echo "POSTFLIGHT FAIL: $lj に meter_version が無い"; POSTFLIGHT_OK=0; fi
done

if [[ "$POSTFLIGHT_OK" -eq 1 ]]; then
  echo "POSTFLIGHT OK: MySQL 生存・probe・接続 baseline・プロセス/スキーマ/tree・manifest すべて通過"
else
  echo "POSTFLIGHT FAILED: 上のいずれかが崩れている。exit 0 だけを見て VERIFIED とみなさないこと"
fi

# ---- MySQL が無くても走る実験 ----
run "19-exp8-guard-fuzz-${stamp}.txt" "⑲ EXP-8 SQL 検査の fuzz（回帰入力ぶんのみ）" \
    go test ./internal/repo/ -run 'TestEXP8|TestGuardProperties' -v -timeout 10m
run "20-exp9-static-analysis-${stamp}.txt" "⑳ EXP-9 保守性の自動検査" \
    go test ./internal/lint/ -run 'TestEXP9|TestAnalyzers|TestEscapeHatch' -v -timeout 10m
# EXP-10 は MYSQL_DSN があれば両エンジンを突き合わせ、無ければ SQLite 側だけを測って
# MySQL 側は UNVERIFIED として残す（測れなかったことを結論にしない）
run "21-exp10-sqlite-${stamp}.txt" "㉑ EXP-10 SQLite companion" \
    go test ./internal/sqlitefacts/ -run TestEXP10 -v -timeout 20m

echo "生ログは ${OUT}/ に残した（git 管理外）。receipt は EXP_RECORD=1 のときだけ docs/results/ に保存"
exit $(( POSTFLIGHT_OK == 1 ? 0 : 1 ))
