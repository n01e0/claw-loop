# Ack Integration Tasklist

このドキュメントは、`claw-loopd` の **OpenClaw delivery ack 連携**を進めるための実行用チェックリスト。
「レポート拡張は一旦止める」方針で、ack連携を最優先で進める。

## 方針
- 優先度1: OpenClaw delivery ack 連携
- 優先度2: 長時間 soak test
- 優先度3: 全面CAS化
- それ以外のレポート機能拡張は、ack連携完了まで原則凍結

## 現在できていること（ベースライン）
- [x] thread-bound run lifecycle (`start/daemon/stop/status/sweep`)
- [x] single-writer lock (`daemon.lock`)
- [x] PR tracking reducer + low-load polling
- [x] notification queue + retry/backoff
- [x] dead-letter + requeue (`--event-id` / `--dry-run`)
- [x] failed reason normalization + histogram/trend
- [x] CI (`fmt/clippy/test/e2e-smoke`)

## Current Execution Plan (approved)
- [x] T1: A2-3の状態遷移ルールを仕様化（遷移表＋禁止遷移を文書化）
- [x] T2: A2-3を実装（遷移ルールをコードへ反映）
- [x] T3: A3-1 unit test追加（A2-3遷移と境界ケース）
- [x] T4: A3-2 e2e拡張（再送/復帰/ack整合）
- [x] T5: docs/tasklist最終更新＋完了報告

## Current Execution Plan v2 (approved: visibility/safety first)
- [x] S1-1: タスク状態モデルを固定（`queued/running/waiting_merge/blocked/done`）
  - `runner-state.json` に `current_task_state` / `current_task_blocked_reason` / `last_task_*` を追加
- [x] S1-2: スレ通知契約を固定（`task_started/task_waiting_merge/task_done/task_blocked` 必須）
- [x] S1-3: `status` 可視化強化（current/last/blocked reason/last PR URL）
- [x] S1-4: 完了判定ガードの回帰強化（`TASK_DONE + PR_URL + merged` 必須）
- [x] S1-5: stuck検知（状態変化なしの待機を通知）
- [x] S1-6: runbook更新（確認ポイント/復旧/手動介入）
  - `docs/runbooks/dogfood-runbook.md`

## Dogfood TODO
- [x] D0-1: tasklist から次の未完了を取得 (`task-next`)
- [x] D0-2: tasklist のチェック状態をCLIで更新 (`task-check`)
- [x] D0-3: runaway guard を追加（`--max-ticks` / `--max-runtime-sec`）
- [x] D0-4: taskループ数ベースの上限を追加（`--max-task-loops`, default 10）
- [x] D1-1: tasklistの「次の未完了」を自動実行する runner エントリ
  - `start --task-runner-cmd` で1タスクずつ実行（進行中タスク完了まで次を開始しない）
  - `task-run-once` で単発実行も可能

## Ack Integration TODO

### Phase 0: 契約定義（先に仕様を固定）
- [x] A0-1: ack の「成功」定義を明文化
  - `openclaw message send` が exit code 0 かつ timeout なしを ack success とする
- [x] A0-2: ack の「失敗」分類を定義
  - `timeout/transport/auth/permission/rate_limited/not_found/upstream_5xx/unknown`
- [x] A0-3: 冪等キーを定義
  - `event_id` を配信・ackの一意キーとして固定

仕様書: `docs/specs/ack-contract.md`

### Phase 1: 実装
- [x] A1-1: `notify-ack.jsonl` を追加（ackイベント履歴）
- [x] A1-2: `flush_notifications` に ack 記録を追加
- [x] A1-3: `delivery-report` に ack情報を統合
  - `acked` / `ack_at` / `ack_error` を表示
- [x] A1-4: `status` に ack集計を追加
  - `acked_total` / `unacked_total` / `last_acked_at`
- [x] A1-5: ack失敗の retry policy を明示化
  - 仕様: `docs/specs/ack-retry-policy.md`

### Phase 2: 安全性
- [x] A2-1: ack記録の二重書き込み防止（event_id idempotency）
  - `(event_id, attempts)` キーで重複ack appendを抑止
- [x] A2-2: daemon再起動跨ぎでのack整合性確認
  - daemon起動時に `delivery_reconciled` で queue/ack を再整合
- [x] A2-3: dead-letter と ack の状態遷移ルールを固定
  - 仕様: `docs/specs/ack-state-transitions.md`

### Phase 3: テスト
- [x] A3-1: unit test（ack分類・遷移）
  - `flush_notifications_*` 系テストで A2-3 遷移と境界ケースを追加
- [x] A3-2: e2e smoke拡張（配信成功/失敗/再送/ack）
  - `scripts/e2e-smoke.sh` に case8/case9（再送/復帰/reconcile+ack整合）を追加
- [x] A3-3: 24h soak test シナリオ追加
  - 仕様: `docs/specs/ack-soak-24h.md`
  - 実行: `scripts/soak-24h.sh`

## 進め方ルール
- 1PRで1テーマ（小さく分割）
- 各PRでこのファイルのチェックを更新
- CI green + e2e pass で次へ進む
- 迷ったら仕様（Phase 0）に戻って先に言語化

## 完了報告（Run: cb3e88b1-d322-467c-b984-64d49f337ac8）
- 完了タスク: T1 / T2 / T3 / T4 / T5
- 反映済み:
  - A2-3 仕様化 + 実装（遷移ガード/terminal判定）
  - A3-1 unit test 追加（遷移/境界ケース）
  - A3-2 e2e 拡張（再送/復帰/ack整合/reconcile）
  - A3-3 24h soak シナリオ追加（`docs/specs/ack-soak-24h.md` / `scripts/soak-24h.sh`）
- 主要検証:
  - `cargo fmt --all -- --check`
  - `cargo test --all --all-features`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `./scripts/e2e-smoke.sh ./target/debug/claw-loopd`

## Current Execution Plan v3 (approved: single status message)
- [x] S2-1: statusメッセージモデル追加（`status_message_id` / `status_updated_at`）
- [x] S2-2: daemon通知を編集更新フローへ変更（started/waiting/progressはedit）
- [x] S2-3: 重要イベントのみ新規投稿（blocked/done/stopped/auto_stopped）
- [x] S2-4: status編集失敗時のフォールバック（再作成 + id再保存）
- [x] S2-5: e2e追加（投稿数削減と重複抑止の検証）
- [x] S2-6: runbook更新（single-status運用と復旧手順）

## Current Execution Plan v4 (approved: docs/ci/refactor)
- [x] S3-1: READMEの日本語を英語へ統一（意味を保ちつつ全体整合）
- [x] S3-2: CIへscript構文チェック追加（`bash -n` を自動実行し失敗時にCI fail）
- [x] S3-3: `src/main.rs` の責務分割（モジュール化）+ 既存挙動維持の回帰テスト追加
- [x] S3-4: 分割後の単体テスト拡充（通知/runner/tasklist系の境界ケース）
- [x] S3-5: runbook/READMEの更新（新しい構成と開発手順を反映）

## Current Execution Plan v5 (approved: completion+edit-notify fixes)
- [x] S4-1: merge確認ガードの堅牢化（`gh pr view` timeout時は即blockedにせず再確認待ちへフォールバック）
- [ ] S4-2: `all_tasks_completed` 通知保証（全タスク完了時に最終通知が欠落しないことを回帰で固定）
- [ ] S4-3: task通知をsingle-status編集ベースへ移行（started/waiting/progressはedit、重要イベントのみ新規投稿）
- [ ] S4-4: e2e + runbook更新（通知欠落/重複/編集失敗フォールバックを検証）

## 次の1手（着手順）
1. S4-1: merge確認ガード堅牢化
2. S4-2: all_tasks_completed通知保証
3. S4-3: task通知のsingle-status編集化
4. S4-4: e2e/runbook更新
