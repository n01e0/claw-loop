# Ack / Dead-letter State Transitions (A2-3)

`claw-loopd` の通知配送における、`dead-letter` と `ack` の状態遷移ルールを固定する。

- 対象データ:
  - `notify-queue.jsonl`
  - `notify-dispatched.jsonl`
  - `notify-ack.jsonl`
  - `notify-dead-letter.jsonl`
- 一意キー: `event_id`
- ack 冪等キー: `(event_id, attempts)`

---

## 1. モデル

通知イベント 1件（`event_id` 単位）の主状態を以下で扱う。

- `queued`: queue 上で配送待ち（`next_retry_at` 未到来/未設定を含む）
- `retry_wait`: queue 上で backoff 待ち（`next_retry_at > now`）
- `dispatched`: 配送成功で終端（`notify-dispatched.jsonl`）
- `dead_letter`: 配送失敗で終端（`notify-dead-letter.jsonl`）

ack は主状態とは別の履歴として記録する。

- `ack.ok=true` (`category=ok`): その attempt の配送成功
- `ack.ok=false` (`category!=ok`): その attempt の配送失敗
- 同一 `(event_id, attempts)` の ack は追記しない（冪等）

---

## 2. 許可遷移（遷移表）

| # | 現在状態 | トリガー | 条件 | 次状態 | ack 記録 | 主な副作用 |
|---|---|---|---|---|---|---|
| T1 | `queued` | `flush_notifications` | `next_retry_at` 未設定 or `<= now` | `dispatched` | `ok=true, category=ok` | attempt(success) 追記、`notify-dispatched` 追記、queue から除外 |
| T2 | `queued` | `flush_notifications` | 配送失敗 + retryable + `attempts < max_attempts` | `retry_wait` | `ok=false, category=<分類>` | attempt(fail) 追記、`next_retry_at` 設定、queue 維持 |
| T3 | `queued` | `flush_notifications` | 配送失敗 + (non-retryable or `attempts >= max_attempts`) | `dead_letter` | `ok=false, category=<分類>` | attempt(fail) 追記、dead-letter 追記、queue から除外 |
| T4 | `retry_wait` | 時刻到達 | `now >= next_retry_at` | `queued` | なし | backoff 待ち解除（次 flush で配送試行対象） |
| T5 | `dead_letter` | `requeue-dead-letter` | `event_id` が queue/dispatched に未存在 | `queued` | なし | dead-letter から除去し queue へ再投入（attempts は保持または reset） |

補足:
- `delivery_reconciled` 実行時、`dispatched` 済み `event_id` が queue に残っていた場合は queue から除去する（状態整合の補正）。
- `notify-ack.jsonl` は起動時 reconcile で `(event_id, attempts)` 重複を除去する。

---

## 3. 禁止遷移

以下は仕様上 **禁止**。実装ではガードで防ぐ。

| 禁止遷移 | 理由 | ガード |
|---|---|---|
| `dispatched -> queued` | 成功済み通知の再配送を防ぐ | flush 時 `dispatched event_id` をスキップ。requeue 時も occupied 判定で拒否 |
| `dispatched -> dead_letter` | 成功後に失敗終端へ落とさない | dead-letter 追加は配送失敗経路のみ |
| `dead_letter -> dispatched`（直接） | 再送監査を保つ（queue を経由させる） | requeue で一度 `queued` に戻す設計 |
| `queued/retry_wait -> dispatched`（ack なし） | 配送成功の監査証跡欠落を防ぐ | success 経路で ack(success) を先に記録してから dispatched 追記 |
| `queued/retry_wait -> dead_letter`（ack なし） | 終端失敗の分類欠落を防ぐ | failure 経路で ack(failure) を先に記録してから dead-letter 追記 |
| 同一 `(event_id, attempts)` の ack 重複追加 | 監査ログ重複防止 | append 時 idempotency check + reconcile 時 dedupe |
| `ack.ok=true` 後の再試行 | 成功終端後に状態を巻き戻さない | `dispatched` イベントは flush 対象外 |

---

## 4. 運用上の注意

- `requeue-dead-letter --reset-attempts` を使うと attempt 番号が再利用される。
  - このとき既存 `(event_id, attempts)` と衝突した ack は追記されない（仕様どおり）。
- ack 履歴の連続性を重視する場合は `--reset-attempts` なし（attempts 維持）を推奨。

---

## 5. 受け入れ基準（A2-3）

- 遷移表（許可遷移）と禁止遷移が文書化されている
- `dead-letter` と `ack` の関係（失敗時 ack 記録 → dead-letter 判定）が明記されている
- 冪等キー `(event_id, attempts)` と重複排除ルールが明記されている
