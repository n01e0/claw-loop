# Ack Integration 24h Soak Test Scenario (A3-3)

`claw-loopd` の ack/dead-letter/reconcile 振る舞いを長時間で検証するための soak シナリオ。

## 目的

- 24時間連続で以下を確認する。
  - ack ログの一意性（`(event_id, attempts)` 重複なし）
  - terminal状態（`dispatched` / `dead_letter`）へ遷移済みイベントが queue に残らない
  - 再送（dead-letter requeue）と daemon 復帰（kill/restart）後に整合性が維持される

## 実行スクリプト

- `scripts/soak-24h.sh`
- デフォルト 24h（`SOAK_DURATION_SEC=86400`）
- OpenClaw配送はモック (`openclaw` shim) を使い、成功/失敗を混在させる

## 実行例

```bash
# build
cargo build

# 24h 実行（デフォルト）
./scripts/soak-24h.sh ./target/debug/claw-loopd

# 短時間ドライラン（例: 2分）
SOAK_DURATION_SEC=120 SOAK_NOTIFY_EVERY_SEC=5 ./scripts/soak-24h.sh ./target/debug/claw-loopd
```

## シナリオ内容

1. `start --deliver-openclaw` で run を開始
2. 一定間隔で `notify` を投入
3. 各ループで `status` を観測し、以下の不変条件を検証
   - `notify-ack.jsonl` に重複キー `(event_id, attempts)` がない
   - `notify-queue.jsonl` に terminal event (`dispatched` / `dead_letter`) が存在しない
4. 定期的に daemon を kill して `daemon` 再起動（復帰シナリオ）
5. 定期的に dead-letter を `requeue-dead-letter` で復帰（再送シナリオ）
6. 最終 status を出力して終了

## 成功条件

- スクリプトが途中 abort せず完走する
- 途中/最終の invariant check でエラーが出ない
- `status` の指標（acked/unacked/dead_letter/pending）が一貫して増減し、破綻しない

## 失敗時の切り分け観点

- 重複ack発生: `notify-ack.jsonl` の `(event_id, attempts)` 衝突
- stale queue発生: `notify-queue.jsonl` に terminal event_id が残存
- 復帰失敗: daemon kill 後の再起動で `daemon lock` / reconcile 異常
- 再送失敗: `requeue-dead-letter` 後に pending -> dispatched/dead_letter へ進まない
