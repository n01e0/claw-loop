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

## Dogfood TODO
- [x] D0-1: tasklist から次の未完了を取得 (`task-next`)
- [x] D0-2: tasklist のチェック状態をCLIで更新 (`task-check`)
- [x] D0-3: runaway guard を追加（`--max-ticks` / `--max-runtime-sec`）
- [x] D0-4: taskループ数ベースの上限を追加（`--max-task-loops`, default 10）
- [x] D1-1: tasklistの「次の未完了」を自動実行する runner エントリ
  - `start --task-runner-cmd` で1tickあたり1タスクを実行
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
- [ ] A2-3: dead-letter と ack の状態遷移ルールを固定

### Phase 3: テスト
- [ ] A3-1: unit test（ack分類・遷移）
- [ ] A3-2: e2e smoke拡張（配信成功/失敗/再送/ack）
- [ ] A3-3: 24h soak test シナリオ追加

## 進め方ルール
- 1PRで1テーマ（小さく分割）
- 各PRでこのファイルのチェックを更新
- CI green + e2e pass で次へ進む
- 迷ったら仕様（Phase 0）に戻って先に言語化

## 次の1手（着手順）
1. A2-3: dead-letter と ack の状態遷移ルール固定
2. A3-1/A3-2: ack遷移テスト強化
3. A3-3: 24h soak test
