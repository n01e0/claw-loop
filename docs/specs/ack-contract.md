# Ack Contract (A0)

`claw-loopd` の OpenClaw delivery ack 連携における最小契約。

## 1. Ack成功の定義 (A0-1)

`deliver_openclaw=true` のとき、以下を満たした場合に **ack success** と判定する。

- `openclaw message send ...` のプロセス終了コードが `0`
- タイムアウトしていない

補足:
- `openclaw` 側から返る message id 等は現段階では必須にしない（将来拡張）。
- `deliver_openclaw=false` の場合はローカル配送成功として扱う（既存互換）。

## 2. Ack失敗分類 (A0-2)

ack failure は以下カテゴリへ正規化する。

- `timeout`
  - 実行タイムアウト
- `transport`
  - 接続失敗/到達不能/DNS解決失敗など
- `auth`
  - unauthorized/token invalid/401
- `permission`
  - forbidden/permission denied/403
- `rate_limited`
  - 429/rate limit
- `not_found`
  - thread/channel not found/404
- `upstream_5xx`
  - 5xx 応答
- `unknown`
  - 上記に該当しない失敗

実装では `last_error` から正規化キーを作り、ackログへ保存する。

## 3. 冪等キー (A0-3)

`event_id` を配信・ackの一意キーとする。

- `notify-queue.jsonl` / `notify-dispatched.jsonl` / `notify-ack.jsonl` / dead-letter で同じ `event_id` を使う
- 同一 `event_id` の ack記録は重複追加しない（idempotent append）
- 再送時も `event_id` は変えない（attemptのみ増加）

## 4. Ack記録（次フェーズで実装）

想定スキーマ（`notify-ack.jsonl`）:

```json
{
  "event_id": "uuid",
  "run_id": "uuid",
  "acked_at": "2026-03-05T10:00:00Z",
  "ok": true,
  "category": "timeout|transport|auth|permission|rate_limited|not_found|upstream_5xx|unknown",
  "error": "optional raw error text"
}
```

`ok=true` の場合 `category=ok` 相当を内部で扱ってもよい（JSON上は省略可）。
