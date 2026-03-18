# Dependency-aware hold classification memo

`claw-loopd` の coarse state は引き続き `running | waiting | blocked | done | stopped` のまま維持しつつ、
直近の dogfood 事例を整理すると、`waiting` / `blocked` の内訳をもう 1 段持たないと判断と通知が混ざる。

このメモでは、直近の block/待機事例（`waiting_merge` 再確認、CI fail、runner shim の誤判定、P2-/P4-系の依存待ち）を 4 つの `hold_kind` に整理し、後続の D2-D7 で使う分類方針を固定する。

---

## 1. 結論

4 つを **top-level state の代替** にはしない。代わりに、既存の coarse state にぶら下がる **secondary classification (`hold_kind`)** として扱う。

```text
status=waiting  -> hold_kind in { waiting_merge, dependency_wait }
status=blocked  -> hold_kind in { real_blocked, runner_failure }
```

つまり:

- `waiting_merge` は `waiting` の一種
- `dependency_wait` も `waiting` の一種
- `real_blocked` は `blocked` の一種
- `runner_failure` も `blocked` の一種

これで、

- 「今は待つべきなのか」
- 「修理 task を積むべきなのか」
- 「そもそも runner/daemon 側の不具合なのか」

を分けて扱える。

---

## 2. 直近事例の整理

### 2-1. `waiting_merge` のまま待つべき事例

#### A. merge 待ちそのもの

- PR 作成済み
- auto-merge もしくは manual-merge fallback で前進可能
- CI pending / merge 未完了なだけ

代表例:

- PR #97: auto-merge 非対応 repo では manual squash merge fallback へ移行
- PR #87 / #114: required checks 未設定は warning を付けるが、状態自体は `waiting_merge`

この種別は **失敗ではなく進行中**。recovery task は不要。

#### B. `waiting_merge` 再確認時の retryable error

代表例:

- PR #100
- `docs/roadmaps/ack-integration-tasklist.md` S4-1

`gh pr view` timeout のような一過性エラーで即 `blocked` へ落とすと、
本当は待てば進むケースまで recovery task 化してしまう。

したがって retryable error は `runner_failure` ではなく、`waiting_merge` の内部 reason として保持する。

**分類:** `status=waiting`, `hold_kind=waiting_merge`

---

### 2-2. `waiting_merge` から `real_blocked` へ遷移すべき事例

#### C. PR はあるが、もう待っても進まない

代表例:

- PR #99: `waiting_merge` 中に CI failure を検出したら block 扱いへ
- PR #102: `mergeStateStatus=DIRTY` を non-progress として block 扱いへ
- PR #92 / #95: auto-merge の arm / re-arm に失敗したら fail-closed で block

共通点:

- すでに PR は存在する
- しかし「待つ」だけでは解決しない
- ブランチ修理、CI 修正、merge conflict 解消などの **作業** が必要

この種別は `waiting_merge` の文脈を保持しつつ、分類は `real_blocked` へ遷移させるべき。

**分類:** `status=blocked`, `hold_kind=real_blocked`, `source=waiting_merge_recheck`

重要なのは、`waiting_merge` を top-level の固定ラベルにしないこと。PR 待ちから始まっても、途中で「実作業が必要な block」に変わりうる。

---

### 2-3. runner がタスク結果を誤読/誤生成した事例

#### D. 実際は完了しているのに blocked 扱いになった

代表例:

- PR #106: 空行先頭で `TASK_DONE` を取り逃した
- PR #108: chatter が先に出て `TASK_DONE PR_URL=...` を取り逃した
- `.ralph/runs/aff3734f-4d05-44e1-b1d6-f3573e06f6e2/events.jsonl`
- `.ralph/runs/815a9c9d-fcdd-4322-ba11-a28b699d32fa/events.jsonl`

これらは見かけ上 `TASK_BLOCKED` 相当の挙動になったが、本質は task の中身ではなく **runner shim / contract parsing の不具合**。

この種別を `real_blocked` に混ぜると、

- 存在しない修理 task を積む
- 本来 fix すべき対象（runner/daemon）を見失う
- downstream task が fake な recovery に引っ張られる

ので、明確に `runner_failure` として分けるべき。

**分類:** `status=blocked`, `hold_kind=runner_failure`

#### E. runner/agent 呼び出し自体の失敗

代表例:

- `openclaw agent` command failed
- GitHub repo 解決不能 / gh 認証不備 / contract 不整合

これも task 本体の失敗ではなく、実行基盤の失敗。

**分類:** `status=blocked`, `hold_kind=runner_failure`

補足: `TASK_WAITING_AGENT_LOCK` のように明示的 retry contract があるものは `blocked` ではなく一時待機扱いのままでよい。つまり **runner 起因でも retryable なら waiting、non-retryable なら runner_failure** である。

---

### 2-4. P2-/P4-系の stacked/phase 依存待ち

#### F. 前段 task / 前段 PR が片付くまで後続 task が安全に始められない

このメモの主題。典型例は:

- 前段 PR が merge されるまで後続 task の branch を切るべきでない
- 前段 task の生成物/契約が確定しないと次 task の実装方針が定まらない
- 「いま blocked に見える」が、実際には upstream completion を待てば自然に再開できる

これは `real_blocked` ではない。

理由:

- 修理対象が current task の不具合ではない
- recovery task を生成しても、本質的には「依存が解決するまで待て」以上にならない
- auto-recover 対象にすると、依存待ちが snowball してノイズ task を増やす

また `waiting_merge` でもない。

理由:

- current task 自身の PR をまだ持っていない場合がある
- 待っている対象は「current task の merge」ではなく「別 task / 別 PR の完了」

したがって dedicated な `dependency_wait` を導入すべき。

**分類:** `status=waiting`, `hold_kind=dependency_wait`

---

## 3. 分類表

| hold_kind | coarse state | 典型例 | 何を待つ/直すか | auto-recover | 次のアクション |
|---|---|---|---|---|---|
| `waiting_merge` | `waiting` | CI pending, auto-merge待ち, manual merge fallback, retryable `gh` timeout | current task の PR 進行 | しない | PR/CI を再確認 |
| `dependency_wait` | `waiting` | P2-/P4-系の前段 task / 前段 PR 待ち | upstream task/PR の完了 | しない | 依存解消イベント待ち |
| `real_blocked` | `blocked` | CI fail, DIRTY PR, merge 不能, missing fixture, approval drift | current task か current PR の修理 | してよい | recovery task 生成 or 手動介入 |
| `runner_failure` | `blocked` | contract parse bug, runner shim bug, `openclaw agent` 実行失敗, gh/auth/tooling failure | runner/daemon/integration の修理 | しない | 実行基盤を直す |

---

## 4. 判定ルール

### Rule 1: 「待てば自然に進む」なら `waiting`

- current PR の merge/CI を待つ → `waiting_merge`
- upstream task / upstream PR の完了を待つ → `dependency_wait`

### Rule 2: 「修理作業が必要」なら `blocked`

- current task/PR を直す必要がある → `real_blocked`
- runner/daemon/tooling を直す必要がある → `runner_failure`

### Rule 3: `waiting_merge` は PR 文脈、`dependency_wait` は upstream dependency 文脈

区別点は「何を待っているか」。

- `waiting_merge`: `current_task_pr_url` が主役
- `dependency_wait`: `depends_on.task_id` / `depends_on.pr_url` が主役

### Rule 4: `waiting_merge` 中に non-progress が判明したら `real_blocked` へ遷移

- failed checks
- dirty merge state
- non-retryable merge arm / merge_now failure

このとき `source=waiting_merge_recheck` は保持するが、分類自体は `real_blocked`。

### Rule 5: fake block を auto-recover しない

- runner が `TASK_DONE` を取り逃した
- agent command 失敗
- repo/gh/tooling 設定不備

は `runner_failure` として止める。`real_blocked` と同じ auto-recover 経路へ入れない。

---

## 5. 後続実装へ落とす shape

D2-D4 では、少なくとも `waiting_reason: String` 一本ではなく、次の構造を durable に持つべき。

```rust
enum TaskHoldKind {
    WaitingMerge,
    DependencyWait,
    RealBlocked,
    RunnerFailure,
}

struct TaskDependencyRef {
    task_id: Option<String>,
    pr_url: Option<String>,
    run_id: Option<String>,
    summary: String,
}

struct TaskHoldContext {
    kind: TaskHoldKind,
    source: TaskHoldSource,     // runner_exit / waiting_merge_recheck / dependency_gate / daemon_preflight
    summary: String,
    detail: Option<String>,
    retryable: bool,

    current_task_id: String,
    current_task_text: Option<String>,
    current_pr_url: Option<String>,

    depends_on: Option<TaskDependencyRef>,
    exit_code: Option<i32>,
    blocked_context: Option<BlockedContext>,
}
```

最低限必要なのは:

- `kind`
- `retryable`
- `current_pr_url`
- `depends_on.*`
- `source`

これがあれば daemon は:

- `waiting_merge` と `dependency_wait` をどちらも `waiting` として見せつつ、通知を分けられる
- `real_blocked` だけ auto-recover 対象にできる
- `runner_failure` を fail-closed で止められる
- 前段 PR merge を契機に `dependency_wait` を自動解除できる

---

## 6. runner/daemon 契約への示唆

D2 で追加する専用契約は、少なくとも「generic blocked ではない dependency wait」を表現できる必要がある。

例えば runner 出力は次のような情報を返せるとよい。

```text
TASK_WAITING_DEPENDENCY TASK_ID=P4-2 DEPENDS_ON_TASK=P4-1 PR_URL=https://github.com/.../pull/123
```

もしくは task id がまだ無く PR 単位で待つなら:

```text
TASK_WAITING_DEPENDENCY PR_URL=https://github.com/.../pull/123
```

ポイントは、`TASK_BLOCKED` の free-form reason に埋めないこと。

dependency wait を文字列推論にすると、

- dedupe が不安定
- 再開条件が取れない
- 「何を待っているか」を operator に明示できない

からである。

---

## 7. 通知方針

### `waiting_merge`

- 「current PR が merge/CI待ち」であることを前面に出す
- warning（required checks missing, retryable timeout）は suffix / detail で出す

### `dependency_wait`

- 「この task は block ではなく依存待ち」であることを明言する
- `depends_on.task_id` / `depends_on.pr_url` / 待機理由を出す
- 人手介入が不要なら、その旨を明記する

### `real_blocked`

- 原因
- 何を直すか
- auto-recover するかどうか

を出す。現在の `BlockedContext` 活用方向を継続。

### `runner_failure`

- task ではなく runner/daemon/tooling 側の問題だと明示する
- recovery task を積まない
- operator が見るべき箇所（runner stderr, gh auth, contract line, daemon logs）を示す

---

## 8. このメモで固定したいこと

1. `dependency_wait` は `blocked` ではなく `waiting`
2. `waiting_merge` は `waiting` の一種であり、CI fail / DIRTY になった時点で `real_blocked` へ遷移する
3. `runner_failure` は `real_blocked` から分離し、auto-recover しない
4. 後続タスクは `waiting_reason: String` 推論ではなく `hold_kind + metadata` を durable 化する

この分類であれば、P2-/P4-系の依存待ちは recovery task 化されず、PR待ち・真の修理待ち・runner不具合が混線しない。

---

## 9. 参考にした直近変更

- PR #92: auto-merge enable failure を fail-closed block 化
- PR #97: auto-merge unavailable 時の manual merge fallback
- PR #99: waiting-merge 中の CI failure を blocked + recovery 対象化
- PR #100: waiting-merge timeout を retryable waiting 化
- PR #102: DIRTY PR を blocked 化
- PR #106 / #108: runner contract parse の誤判定修正
- `.ralph/runs/aff3734f-4d05-44e1-b1d6-f3573e06f6e2/events.jsonl`
- `.ralph/runs/815a9c9d-fcdd-4322-ba11-a28b699d32fa/events.jsonl`
