# Dogfood Runbook (S1-6)

`claw-loopd` の dogfood 運用時に、状態確認・復旧・手動介入を行うための実運用メモ。

## 1) 確認ポイント（通常監視）

### A. まず `status` を確認

```bash
cargo run -- status --repo . --run-id <RUN_ID>
```

主な見る項目:

- `status` / `summary` / `waiting_reason`
- `runner.current`（現在のタスク）
  - `id`, `state`, `blocked_reason`, `pr_url`
- `runner.last`（直近タスク）
  - `id`, `state`, `reason`, `pr_url`
- `last_pr_url`（現在または直近PR URL）
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
- `runner.waiting_unchanged_ticks` が閾値超過
  - `task_waiting_stuck` 通知済み。PR/CI進捗の手動確認へ

### C. 補助確認

```bash
cargo run -- delivery-report --repo . --run-id <RUN_ID> --status all --limit 20
cargo run -- sweep --repo . --run-id <RUN_ID>
```

## 2) 復旧手順（よくあるケース）

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

### ケース3: dead-letter 増加

```bash
cargo run -- delivery-report --repo . --run-id <RUN_ID> --status failed --limit 20
cargo run -- requeue-dead-letter --repo . --run-id <RUN_ID> --event-id <EVENT_ID> --limit 1 --reset-attempts
```

- 短期の送信障害なら requeue で復帰
- 恒久失敗（権限/宛先不備）は設定修正を先行

### ケース4: daemon停止/孤児化

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
- `delivery-report` で failed/pending が許容範囲
- 必要なら thread 通知（`task_blocked` / `task_waiting_stuck` への対応内容）を残す
