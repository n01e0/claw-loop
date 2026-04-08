# Failure-first backlog detector / task selection-gating 設計メモ

このメモは、C14 系で入れる failure-first 制御について、現状の連携点と制御点を棚卸しし、後続実装の最小設計を固定するためのもの。

対象:

- `src/main.rs`
- `scripts/rl-task-agent.sh`
- `README.md`
- `docs/runbooks/dogfood-runbook.md`

非目標:

- この PR では実装変更しない
- backlog detector 自体の実装方式までは確定しない
- tasklist 記法の全面刷新までは扱わない

---

## 1. 先に結論

failure-first を効かせる主制御点は **daemon 側の next-task selection** である。

理由:

- `scripts/rl-task-agent.sh` が起動する時点では、すでに daemon が「どの task を実行するか」を決めている
- したがって backlog>0 のとき feature 系 task へ進ませない主責務は `select_next_task_entry(...)` とその呼び出し側に置くべき
- task-agent 側は主制御ではなく **第2ガード** として backlog 状態を読めるようにし、誤選択時に `TASK_WAITING_DEPENDENCY` / `TASK_BLOCKED` を返せるようにするのがよい

推奨方針は次の 3 点。

1. backlog detector の結果を daemon 起動中に読める **repo-bound な snapshot** として扱う
2. `current_task_id.is_none()` で次 task を選ぶ直前に、recovery 優先の上へ **failure-first gate** を差し込む
3. backlog>0 なのに修正系 task を選べない場合は feature へ進まず、`waiting` か `blocked` を明示する

---

## 2. 現状の制御フロー

### 2-1. task selection はいま非常に薄い

現在の next task selection は `src/main.rs` の `select_next_task_entry(...)` にほぼ集約されている。

現状ロジックは:

1. `preferred_recovery_task_id` があればその open task を優先
2. なければ先頭の未完了 task を返す

つまり今は:

- backlog detector 入力なし
- task kind（修正系 / feature 系）の概念なし
- repo 境界付きの external gate なし
- policy 上の「この task は今やってよいか」の判定なし

で、**open task を上から順に取るだけ** になっている。

### 2-2. daemon 側の主な分岐点

daemon loop で failure-first に効く制御点は次の通り。

| 制御点 | 現状の役割 | failure-first で触るべき点 |
|---|---|---|
| `if runner_state.current_task_id.is_none()` | 新しい task を選ぶ入口 | **最重要**。ここで backlog gate を評価する |
| `select_next_task_entry(...)` | recovery task 優先 + 先頭未完了 task 選択 | 修正系/feature 系の分類と backlog>0 時の候補絞り込みを追加 |
| `runner_state.preferred_next_task_id` | dependency 解消後や follow-up rerun の優先候補 | backlog gate より下位の「希望順」に落とす必要がある |
| `runner_state.auto_recover_last_task_id` | auto-recover 直後の recovery task 優先 | repair 系として最優先のままでよい |
| `WaitingDependencyProgress::Resolved` 後の rerun | 同じ task を再開する | 依存解消後も、rerun 対象が feature 系なら gate で止められるようにする |
| `state.status` / `state.waiting_reason` 更新 | operator 向け可視化 | backlog gate 専用の waiting/block reason を入れる |
| `queue_notification(...)` | task_waiting / blocked 通知 | backlog gate 専用の通知文言を追加する |

### 2-3. runner 側は task 選択ではなく task 実行契約を持つ

`scripts/rl-task-agent.sh` の責務は現在:

- approved dogfood prompt を agent へ渡す
- task file hash drift を検知する
- `TASK_DONE` / `TASK_WAITING_MERGE` / `TASK_WAITING_DEPENDENCY` / `TASK_BLOCKED` を解釈する
- PR auto-merge / manual-merge fallback / CI failure を処理する

ここには **backlog detector の読み取りや task kind 判定はまだ無い**。

また、runner 起動時点では `CLAW_TASK_ID` / `CLAW_TASK_TEXT` が確定済みなので、runner だけで failure-first を実現するのは遅い。

したがって runner 側は:

- daemon が選んだ task を再評価する第2ガード
- backlog detector が active な間は feature 系 task を「実行しない」と返す保険

として使うのが自然。

---

## 3. 現状の durable state で足りているもの / 足りないもの

### 3-1. 既存 state で流用できるもの

すでに使えるもの:

- `State.status`
- `State.waiting_reason`
- `RunnerState.preferred_next_task_id`
- `RunnerState.current_task_state`
- `RunnerState.current_waiting_dependency`
- `RunnerState.last_waiting_dependency`
- `tracked_task_pr_urls`

特に `waiting_dependency` は、

- 「いまは自然待機である」
- 「auto-recover しない」
- 「依存が解消したら再開する」

という failure-first と近い表現をすでに持っている。

### 3-2. そのままでは足りないもの

不足しているのは次の構造。

#### A. backlog detector の durable な入力表現

今は manifest / state / runner-state のどこにも:

- backlog count
- detector freshness
- detector source
- detector がこの repo 用かどうか
- detector error/stale 状態

を表す場所がない。

#### B. task の「修正系 / feature 系」分類

今の `TaskChecklistEntry` は:

- `id`
- `text`
- `done`

しか持たない。

つまり daemon は task を選ぶ瞬間に、

- repair として進めてよい task
- backlog 中は止めるべき feature task
- docs / infra のような中立 task

を識別できない。

#### C. backlog gate 自体の operator-facing な状態

`waiting_reason` は文字列 1 本なので、後続タスクで必要になる:

- backlog active により意図的に待っているのか
- detector が壊れて fail-closed で止まっているのか
- backlog count はいくつなのか
- どの detector snapshot を見たのか

が durable に残らない。

---

## 4. backlog detector 連携点の棚卸し

### 4-1. どこで detector を読むべきか

推奨は **daemon が task selection のたびに detector snapshot を読む** 方式。

理由:

- selection 直前の最新 backlog を使える
- runner まで起動してから feature task を reject する無駄が減る
- `waiting_merge` / `waiting_dependency` 解消後の再選択にも同じ policy をかけられる

実装上の入口候補は:

- `start` 時に detector source を manifest へ保存
- daemon tick 中、`current_task_id.is_none()` に入った時点で snapshot を読む
- `select_next_task_entry(...)` へ snapshot を渡す

### 4-2. detector snapshot に最低限必要な情報

failure-first gating に最低限必要なのは次。

```text
repo identity     // この repo 向け結果か
status            // clear | backlog | stale | error
backlog_count     // 0 / >0
summary           // operator 向け一行説明
updated_at        // freshness 判定用
```

補助的にあるとよいもの:

```text
source            // どの detector 出力か
details[]         // backlog items の短いサマリ
```

重要なのは、**repo identity を必須にすること**。

タスクの constraint にある通り、cross-repo backlog をこの loop に混ぜないため、snapshot は少なくとも repo path か repo slug を持つべき。
repo 不一致なら fail-closed にして feature task へ進めない。

### 4-3. stale / error の扱い

推奨分類:

- detector `status=clear` かつ `backlog_count=0` -> gate open
- detector `status=backlog` かつ `backlog_count>0` -> repair-only selection
- detector `status=stale|error|repo_mismatch|missing` -> **blocked**

理由:

- backlog active は policy 上の自然待機なので `waiting` でよい
- detector が壊れているのは、policy 判定ができない基盤異常なので `blocked` がよい

---

## 5. task selection / gating 制御点の棚卸し

### 5-1. いちばん大事な制御点

最重要の差し込み位置はここ。

```text
if runner_state.current_task_id.is_none() {
    load_task_checklist(...)
    select next task
}
```

ここでやるべきことは:

1. backlog snapshot を読む
2. open task 一覧を task kind 付きで評価する
3. recovery / repair / feature の優先順で次 task を選ぶ
4. 選べない場合は waiting/block 状態へ落とす

### 5-2. `preferred_next_task_id` は絶対優先ではなく「候補ヒント」に下げる

現状は `preferred_next_task_id` が open ならその task を優先する。

ただし failure-first 後は、たとえば:

- feature task が dependency wait していた
- 依存が解消した
- `preferred_next_task_id` にその feature task が入る
- その間に backlog>0 になった

というケースが起こる。

このときでも feature task を即 rerun すると policy 破りになる。

したがって `preferred_next_task_id` は:

- recovery/repair candidate の中で優先するヒント
- backlog gate を飛び越える bypass ではない

という扱いに変えるべき。

### 5-3. auto-recover との優先順

`auto_recover_last_task_id` は blocked task 由来の recovery task を指す。
これは本質的に修正系なので、failure-first と整合する。

推奨優先順:

1. open な recovery task（`preferred_next_task_id` / `auto_recover_last_task_id` が repair 扱いのもの）
2. その他の open な repair task
3. backlog=0 のときだけ feature / neutral task
4. backlog>0 で repair task 不在なら waiting or blocked

### 5-4. task kind 判定の置き場所

短期的には `TaskChecklistEntry.text` から分類するしかない。

推奨は 2 段構え。

#### 第1候補: 明示タグ

task text 先頭にたとえば:

- `[repair]`
- `[feature]`
- `[neutral]`

のようなタグを置けるようにする。

利点:

- daemon が機械的に判定できる
- task selection と operator 意図が揃う
- 後続の docs/runbook にも落としやすい

#### 第2候補: 互換用ヒューリスティクス

既存 tasklist 向けには、タグが無い場合だけ補助的に:

- `fix`, `bug`, `regression`, `recover`, `repair`, `guard`, `stabilize` などは repair 寄り
- `feature`, `add`, `introduce`, `build` などは feature 寄り
- 判定不能は `unknown`

として扱う。

そして **backlog>0 で `unknown` は feature 側として止める** のが fail-closed。

---

## 6. task-agent 側の第2ガード

### 6-1. なぜ task-agent にも detector を読ませるか

daemon だけで選択制御するのが主筋だが、task-agent 側にも backlog 状態を渡しておく価値がある。

理由:

- daemon 側の分類漏れや将来のバグに対する safety net になる
- agent 自身が「この task は isolated green PR にすべきでない」と判断しやすくなる
- feature task を誤って選んだ場合でも、runner contract で明示待機へ戻せる

### 6-2. runner への渡し方

`scripts/rl-task-agent.sh` に追加する連携点候補:

- detector snapshot path を env で渡す
- もしくは daemon が要約済み値を env で渡す
  - `CLAW_BACKLOG_STATUS`
  - `CLAW_BACKLOG_COUNT`
  - `CLAW_BACKLOG_SUMMARY`
  - `CLAW_BACKLOG_UPDATED_AT`

そのうえで prompt にも短く埋め込む。

### 6-3. task-agent に期待する返し方

推奨:

- backlog>0 で、この task が standalone green PR にすべきでないと判断したら `TASK_WAITING_DEPENDENCY` ではなく専用 backlog gate が無い限り `TASK_BLOCKED` か `TASK_WAITING_DEPENDENCY` を慎重に使う
- ただし backlog は「特定 upstream task/PR を待つ」わけではないことが多いので、既存 contract では `TASK_WAITING_DEPENDENCY` に無理やり載せない方がよい

つまり task-agent 側は暫定的には:

- dependency target を具体化できるなら `TASK_WAITING_DEPENDENCY`
- できないなら `TASK_BLOCKED: failure-first backlog gate active ...`

がよい。

本命は daemon 側に **backlog-gated waiting** を持たせ、runner にその分岐を背負わせすぎないこと。

---

## 7. operator-facing state の推奨

### 7-1. backlog active だが detector は健全

この場合は `waiting` がよい。

推奨文面例:

```text
summary: task selection gated by failure-first backlog policy
waiting_reason: backlog gate active: backlog_count=3; repair tasks only
```

通知も:

- generic blocked ではない
- detector は正常
- いまは feature を進めないという policy wait
- repair task を追加するか、backlog を解消すると再開する

を明示するべき。

### 7-2. detector missing/stale/error/repo mismatch

この場合は `blocked` がよい。

推奨文面例:

```text
summary: backlog detector unavailable
waiting_reason: failure-first gate blocked: backlog snapshot missing or stale for repo <repo>
```

こちらは operator に対して:

- detector を直す
- repo 対応を確認する
- snapshot freshness を回復する

という基盤介入を促すべき。

---

## 8. 後続実装へ向けた最小設計

### 8-1. 追加したい内部概念

後続実装では、少なくとも次を入れるとよい。

```text
BacklogSnapshot
TaskExecutionKind = Repair | Feature | Neutral | Unknown
SelectionGateResult = Open(next_task) | Waiting(reason) | Blocked(reason)
```

これで:

- detector 読み取り
- task kind 判定
- selection policy の結果

を分離できる。

### 8-2. 変更順のおすすめ

#### C14-2

- detector snapshot reader を追加
- `select_next_task_entry(...)` を repair-aware に拡張
- backlog>0 時の waiting/block state を追加
- `preferred_next_task_id` を gate 下の hint 扱いへ変更
- unit test を追加

#### C14-3

- waiting_merge / blocked / dependency_wait との guardrail 衝突を整理
- backlog gate が generic blocked / dependency wait を誤分類しない regression を追加

#### C14-4

- tasklist authoring で `[repair]` / `[feature]` 等をどう使うか docs/runbook/skill に反映
- dogfood で backlog>0 / backlog=0 の両ケースを固定

---

## 9. テスト観点

最低限必要な回帰は次。

### unit

- backlog=0 -> 従来通り recovery 優先 + first-open fallback
- backlog>0 + repair taskあり -> repair task を選ぶ
- backlog>0 + feature taskしかない -> waiting へ落ちる
- backlog>0 + `preferred_next_task_id=feature` + repair taskあり -> repair task を選ぶ
- backlog>0 + `preferred_next_task_id=feature` + repair taskなし -> waiting/block
- detector stale/error/repo mismatch -> blocked

### e2e / smoke

- feature task 実行後ではなく **実行前** に gate される
- waiting_dependency 解消後の rerun でも backlog gate が効く
- auto-recover task は backlog 中でも優先される
- `waiting_merge` 中 task の既存挙動は壊さない
- status / notification で backlog gate 理由が見える

---

## 10. まとめ

現状の `claw-loopd` は、recovery task 優先までは持っているが、backlog detector を使った **policy-driven task selection** はまだ持っていない。

failure-first を入れる主制御点は daemon の next-task selection であり、特に:

- `current_task_id.is_none()` の入口
- `select_next_task_entry(...)`
- `preferred_next_task_id` の扱い
- operator 向け waiting/block reason

が今回の本丸になる。

runner (`scripts/rl-task-agent.sh`) は主制御ではなく第2ガードとして backlog 状態を読めるようにし、誤選択時に fail-closed で返せるようにするのがよい。

要するに後続実装は、

- detector snapshot を repo-bound に読む
- repair / feature を区別して選ぶ
- backlog>0 で repair を選べなければ feature へ進まない
- detector 異常時は blocked で止める

の 4 点を守れば、今回の failure-first 目的を満たせる。
