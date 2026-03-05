# Ack Retry Policy (A1-5)

`claw-loopd` の ack失敗時リトライ方針。

## ポリシー概要

- 判定キー: `ack.category`
- 主要入力: `attempts`（1始まり）
- 上限: `CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS`（default: 5）

## カテゴリ別ルール

### retryable
- `timeout`
- `transport`
- `upstream_5xx`
- `unknown`

ルール:
- `max_attempts = CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS`
- backoff: `5, 5, 15, 30, 60...` 秒

### retryable (rate-limit専用)
- `rate_limited`

ルール:
- `max_attempts = CLAW_LOOPD_DELIVERY_MAX_ATTEMPTS`
- backoff: `30, 30, 60, 120, 300...` 秒

### non-retryable
- `auth`
- `permission`
- `not_found`

ルール:
- 追加リトライなし
- `max_attempts = 1`
- 初回失敗で dead-letter へ移動

## 状態遷移

1. 送信失敗時に `ack.category` を分類
2. retryable かつ上限未満なら queue に戻し `next_retry_at` を設定
3. それ以外は dead-letter へ移動

## 観測点

- `notify-ack.jsonl` に失敗カテゴリを記録
- `notify-dead-letter.jsonl` に終端失敗を記録
- `status` / `delivery-report` の `ack_retry_policy` で有効方針を確認可能
