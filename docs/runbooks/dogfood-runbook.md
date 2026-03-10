# Dogfood Runbook (S2-6)

`claw-loopd` の dogfood 運用時に、single-status 通知モデルを前提に状態確認・復旧・手動介入を行うための実運用メモ。

## 0) single-status 通知契約（運用前提）

### A. 通知モード

- **EditStatus（既存 status メッセージ更新）**
  - started / waiting / progress 系イベント
  - 実装上は「重要イベント以外」を edit 対象として扱う
- **Send（新規投稿）**
  - `blocked` / `done` / `stopped` / `auto_stopped`
  - 同等の重要イベント（`orphan_blocked` / `pr_closed` / `all_tasks_completed`）

### B. 状態保持

`runner-state.json` / `status` 出力に以下が乗る:

- `runner.status_message_id`
- `runner.status_updated_at`

初回の status 通知は `send` で作成し、以後は `status_message_id` に対して `edit` で更新する。

### C. フォールバック

status edit が失敗した場合:

1. 同じ通知内容を `send` で再作成
2. 新しい `messageId` を `status_message_id` として保存
3. `events.jsonl` に `notify_status_edit_fallback_send` を記録

### D. 実装構成（S3以降）

- `src/main.rs`: CLI入口 + daemonオーケストレーション
- `src/notify_policy.rs`: 通知方針（send/edit）、retry/backoff、エラー正規化
- `src/tasklist.rs`: tasklist の parse/count/update

機能追加時は、責務を持つモジュール側へ実装し、`main.rs` は配線・制御に限定する。

## 1) 確認ポイント（通常監視）

### A. まず `status` を確認

```bash
cargo run -- status --repo . --run-id <RUN_ID>
```

主な見る項目:

- `status` / `summary` / `waiting_reason`
- `runner.current`（現在タスク）
  - `id`, `state`, `blocked_reason`, `pr_url`
- `runner.last`（直近タスク）
  - `id`, `state`, `reason`, `pr_url`
- single-status 健全性
  - `runner.status_message_id`
  - `runner.status_updated_at`
- stuck検知
  - `runner.waiting_unchanged_ticks`
  - `runner.waiting_last_notified_ticks`
  - `runner.waiting_stuck_threshold`
- delivery健全性
  - `pending_notifications`, `dead_letter_total`, `acked_total`, `unacked_total`

### B. 判定の目安

- `status=waiting` + `runner.current.state=waiting_merge`
  - PR merge待ちの通常状態
- `status=blocked`
  - 介入が必要（`waiting_reason` と `runner.current.blocked_reason` を優先確認）
- `runner.status_message_id` が空
  - まだ status メッセージ未作成、または失効直後
- `runner.waiting_unchanged_ticks` が閾値超過
  - `task_waiting_stuck` 通知済み。PR/CI進捗の手動確認へ

### C. 補助確認

```bash
cargo run -- delivery-report --repo . --run-id <RUN_ID> --status all --limit 20
cargo run -- sweep --repo . --run-id <RUN_ID>
```

## 2) 復旧手順（single-status 運用）

### ケース1: waiting_merge が長時間変わらない

1. `status.last_pr_url` または `runner.current.pr_url` を確認
2. PR状態確認（merged / closed / checks）
3. 既に merge 済みなら tasklist 反映を確認
4. 進捗なしなら `task_waiting_stuck` の reason をもとに担当へ連絡

### ケース2: blocked（完了判定ガード違反）

想定例: `TASK_DONE` だが `PR_URL` なし、または PR未merge。

1. `waiting_reason` を確認
2. タスク実行側の出力を修正（`TASK_DONE PR_URL=<url>` を厳守）
3. PRが merge 済みか確認
4. 必要なら同タスクを再実行（runner再開）

### ケース3: status edit 失敗（message id失効/権限変化）

1. `events.jsonl` で `notify_status_edit_fallback_send` の有無を確認
2. `runner.status_message_id` が新しいIDに更新されたか確認
3. `delivery-report --status failed` に edit系失敗が残っていないか確認

補助コマンド:

```bash
jq -c 'select(.kind=="notify_status_edit_fallback_send")' .ralph/runs/<RUN_ID>/events.jsonl | tail
```

### ケース4: `runner.status_message_id` が空のまま

1. daemonが `deliver_openclaw=true` で動作しているか確認
2. `pending_notifications` / `delivery-report` で送信失敗の有無確認
3. 手動で progress 通知を1回投入して status message bootstrap を促す

```bash
cargo run -- notify --repo . --run-id <RUN_ID> --kind progress --message "status bootstrap"
```

### ケース5: dead-letter 増加

```bash
cargo run -- delivery-report --repo . --run-id <RUN_ID> --status failed --limit 20
cargo run -- requeue-dead-letter --repo . --run-id <RUN_ID> --event-id <EVENT_ID> --limit 1 --reset-attempts
```

- 短期の送信障害なら requeue で復帰
- 恒久失敗（権限/宛先不備）は設定修正を先行

### ケース6: daemon停止/孤児化

```bash
cargo run -- sweep --repo . --run-id <RUN_ID>
cargo run -- status --repo . --run-id <RUN_ID>
```

- `blocked_orphan` なら daemon 再起動可否を判断
- 再起動時は既存 run dir / task state を保持して復帰

## 3) 手動介入（オペレーター操作）

### A. 安全停止

```bash
cargo run -- stop --repo . --run-id <RUN_ID>
# 即時停止が必要なら
cargo run -- stop --repo . --run-id <RUN_ID> --immediate
```

### B. tasklist 手動反映

```bash
cargo run -- task-check --file docs/roadmaps/ack-integration-tasklist.md --id <TASK_ID> --done true
```

### C. 追加通知の手動送信

```bash
cargo run -- notify --repo . --run-id <RUN_ID> --kind progress --message "manual intervention applied"
```

### D. stuck閾値の調整

- 既定: `CLAW_LOOPD_STUCK_WAIT_TICKS=30`
- 例（短めに検知したい時）:

```bash
CLAW_LOOPD_STUCK_WAIT_TICKS=10 cargo run -- start ...
```

## 4) 介入後の完了条件

- `status` が意図した状態に遷移（`waiting_merge` / `running` / `done` など）
- `runner.current` / `runner.last` が最新状態に整合
- `runner.status_message_id` が安定して保持され、`status_updated_at` が更新される
- `delivery-report` で failed/pending が許容範囲
- 必要なら thread 通知（`task_blocked` / `task_waiting_stuck` 対応内容）を残す

## 5) 開発時チェックリスト（PR前）

S3以降の標準確認手順:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features
find scripts -type f -name '*.sh' -print0 | xargs -0 -r -n1 bash -n
./scripts/e2e-smoke.sh ./target/debug/claw-loopd
```

テスト追加の方針:

- 通知ポリシー境界は `src/notify_policy.rs` の unit test に追加
- tasklist境界は `src/tasklist.rs` の unit test に追加
- runner/daemonの遷移境界は `src/main.rs` の unit test か e2e に追加

S4-4で追加した e2e 観点（`scripts/e2e-smoke.sh` case11）:

- **通知欠落防止**: 全タスク完了 run で `all_tasks_completed` が必ず1回 dispatch される
- **通知重複抑止**: `task_started` / `task_done` / `all_tasks_completed` の kind 別 dispatch 件数が想定どおり（各1件）
- **編集失敗フォールバック**: status edit 失敗時に `notify_status_edit_fallback_send` が記録され、`runner.status_message_id` が新IDへ更新される
