# Blocked / auto-recover 現行データフロー整理メモ

このメモは、`blocked` 発生時に runner から得られている情報がどこまで運ばれ、どこで欠落するかを整理し、後続タスクで必要になる内部表現の設計方針を固定するためのもの。

対象コード:

- `scripts/rl-task-agent.sh`
- `src/main.rs`
- `src/tasklist.rs`

非目標:

- この PR では実装変更は行わない
- 通知文面の改善そのものは後続タスクで扱う

---

## 1. いまの blocked / auto-recover フロー

### 1-1. runner blocked 経路

1. `scripts/rl-task-agent.sh` が失敗時に `TASK_BLOCKED: ...` を返す
   - 典型的には `stderr` に出す
   - `TASK_DONE` 後の auto-merge / CI failure も同じく `TASK_BLOCKED: ...` に畳まれる
2. `run_task_once` が runner の `stdout` / `stderr` を **フル文字列で** `TaskRunOutcome` に保持する
3. daemon loop は `task_runner_tick` event を書く
   - ただし `stdout` / `stderr` はここで `clip_text(..., 1000)` される
4. daemon は `blocked_reason_from_runner(stderr, stdout)` で blocked reason を 1 本の短い文字列へ潰す
   - 優先順は `stderr` の先頭非空行 → `stdout` の `TASK_*` 行 → `stdout` の末尾非空行
   - ここでも `clip_text(..., 200)` が入る
5. その短い reason を `state.waiting_reason` と `runner_state.current_task_blocked_reason` / `last_task_reason` に保存する
6. `format_task_blocked_notification` が blocked 通知を作る
   - 改行を潰して summary 化
   - 240 文字 summary / 600 文字 detail に再度 clip
7. `maybe_auto_recover_blocked_task` が auto-recover を判断する
   - 入力として使うのは `state.waiting_reason`（= すでに短文化済みの文字列）
8. `append_recovery_task_for_blocked` が recovery task を tasklist に追記する
   - `state.waiting_reason` をさらに 160 文字へ clip して task 文面に埋め込む
9. auto-recover を積んだら daemon は `current_task_*` / `current_task_blocked_reason` をクリアし、`state.waiting_reason` も汎用文言へ上書きする
10. follow-up 側に残るのは主に
   - `runner_state.last_task_reason`（短い reason）
   - `task_runner_tick` event 上の clip 済み `stdout` / `stderr`
   - tasklist 上の recovery task 行

### 1-2. waiting_merge blocked 経路

`waiting_merge` 再確認時の blocked は少し別物で、runner の `stdout` / `stderr` は存在しない。

1. daemon が `check_waiting_merge_progress` で PR 状態を再確認する
2. `WaitingMergeProgress::Blocked(reason)` を作る
3. 以降の処理は runner blocked と同じで、`state.waiting_reason` に reason を入れて `maybe_auto_recover_blocked_task` へ渡す

つまり現状の auto-recover は、

- runner 由来の blocked
- daemon 側で再構成した waiting_merge blocked

をどちらも **ただの `state.waiting_reason: String`** として扱っている。

---

## 2. 情報ごとの保持/欠落ポイント

| 情報 | 最初に存在する場所 | durable に残る場所 | 欠落する地点 |
|---|---|---|---|
| blocked reason の全文/複数行 | `TaskRunOutcome.stdout/stderr` | なし（event には clip 済みのみ） | `blocked_reason_from_runner` で 1 行へ潰した時点 |
| runner stdout 全文 | `TaskRunOutcome.stdout` | `task_runner_tick.extra.stdout` に 1000 文字 clip 版のみ | `task_runner_tick` 書き込み後。state / runner-state には保存されない |
| runner stderr 全文 | `TaskRunOutcome.stderr` | `task_runner_tick.extra.stderr` に 1000 文字 clip 版のみ | `task_runner_tick` 書き込み後。state / runner-state には保存されない |
| blocked reason の要約 | `blocked_reason_from_runner(...)` の戻り値 | `state.waiting_reason`, `runner_state.current_task_blocked_reason`, `runner_state.last_task_reason` | auto-recover 後に `current_*` は消える。残るのは短い last reason のみ |
| auto-recover dedupe 用 reason key | `normalize_blocked_reason_for_recovery(state.waiting_reason)` | `runner_state.auto_recover_last_reason` | 元 reason がすでに短文化済みなので、prefix 衝突や詳細欠落を吸収できない |
| recovery task text | `append_recovery_task_for_blocked` の戻り値 (`TaskChecklistEntry.text`) | tasklist の行そのもの | `runner-state.json` / `state.json` / `task_blocked_auto_recovered` event に保存されない |
| recovery decision の説明材料 | `state.waiting_reason` + `recovery_task` の一時変数 | `task_blocked_auto_recovered` event には task id / line と blocked_reason のみ | stdout / stderr / recovery task text / guard判断根拠が follow-up 用にまとまって残らない |

---

## 3. どこで何が失われるか

### A. reason が失われる場所

#### A-1. `blocked_reason_from_runner`

ここが最初の大きな圧縮点。

- `stderr` の先頭 1 行しか見ない
- `stderr` に複数行の説明があっても落ちる
- `stdout` に詳細があっても、`stderr` 先頭が存在すれば採用されない
- 200 文字で再度 clip する

結果として、runner が持っていた:

- reason の全文
- 補足行
- stdout/stderr のどちらに載っていたか
- 複数行の文脈

がここでほぼ失われる。

#### A-2. `format_task_blocked_notification`

ここでは loss というより user-facing summary への再圧縮が起きる。

- 改行は潰れる
- 240/600 文字へ再 clip

blocked 通知としては妥当でも、後続の recovery decision に再利用するには弱い。

### B. stdout / stderr が失われる場所

#### B-1. `task_runner_tick` event

`TaskRunOutcome` から full `stdout` / `stderr` を受け取っているのに、event 化するとき 1000 文字 clip 版しか残さない。

現状これ自体は「監査用に軽く残す」目的としては理解できるが、問題はその後の auto-recover ではこの event を参照しないこと。

つまり、runner 出力は:

- in-memory では一瞬だけフルで存在する
- event では clip 版になる
- state / runner-state / notification / recovery event には入らない

という流れになっている。

#### B-2. auto-recover 直前/直後の state 更新

auto-recover を積むと:

- `runner_state.current_task_blocked_reason = None`
- `runner_state.current_task_pr_url = None`
- `state.waiting_reason = "auto-recovery generated from blocked task ..."`

に切り替わる。

blocked 時点の runner 出力へ辿る手がかりは `events.jsonl` の clip 版しか残らない。

### C. recovery task text が失われる場所

`append_recovery_task_for_blocked` は `TaskChecklistEntry` を返しており、その場では

- `recovery_task.id`
- `recovery_task.line_no`
- `recovery_task.text`

を全部持っている。

しかし `maybe_auto_recover_blocked_task` は durable 側へ:

- `runner_state.auto_recover_last_task_id`
- `task_blocked_auto_recovered` event の `recovery_task_id` / `recovery_task_line`
- 通知文 `auto-recovery task queued: <id> ...`

しか残していない。

そのため recovery task text は:

- tasklist ファイルを直接読む
- その場で通知を組み立てる

以外では再利用できない。

### D. recovery decision 自体が失われる場所

auto-recover の判断は本来、少なくとも次を含んでいる。

- blocked reason
- guard に引っかかったかどうか
- 引っかからなければどんな recovery task を積んだか
- attempts / same-reason count

しかし durable に残るのは現在:

- halted 側: `task_blocked_auto_recover_guard_hit` event
- recovered 側: `task_blocked_auto_recovered` event
- state 側: 汎用 summary / waiting_reason

のみで、通知や status からその判断材料を 1 つのまとまった文脈として再取得できない。

---

## 4. 根本原因

### 4-1. auto-recover の入力が `String` に縮退している

`maybe_auto_recover_blocked_task` が受け取る blocked 文脈は実質:

- `blocked_task_id`
- `state.waiting_reason`

だけ。

この時点で「reason と raw outputs を分けて持つ」余地がない。

### 4-2. human-facing summary と machine-facing context が同じ箱を使っている

現状の `state.waiting_reason` は:

- status 表示
- blocked 通知
- auto-recover dedupe
- recovery task text 生成

に兼用されている。

そのため、ユーザー向けに短くした文字列が、そのまま内部判断や後続 task 生成にも流れてしまう。

### 4-3. blocked 後の follow-up に必要な情報が state に残らない

follow-up 通知で欲しいのは典型的に:

- 原因
- stdout / stderr の要点
- 実際に積んだ recovery task 本文
- auto-recover 継続/停止の判断

だが、現状の durable state にはその箱がない。

---

## 5. 後続タスク向けの設計方針

### 5-1. `BlockedContext` を導入する

少なくとも次の粒度で blocked 文脈を 1 つの構造体にまとめて保持する。

```rust
struct BlockedContext {
    task_id: String,
    task_text: Option<String>,
    pr_url: Option<String>,
    source: BlockedSource,          // runner_exit / waiting_merge / approval / orphan ...
    exit_code: Option<i32>,
    blocked_at: DateTime<Utc>,

    reason_summary: String,         // 通知/status向け短文
    reason_detail: Option<String>,  // multiline を許す詳細

    runner_stdout_excerpt: Option<String>,
    runner_stderr_excerpt: Option<String>,

    recovery_hint: Option<String>,
    recovery_task: Option<RecoveryTaskContext>,
    auto_recover: Option<AutoRecoverDecision>,
}
```

重要なのは:

- `reason_summary` と `reason_detail` を分ける
- `stdout` / `stderr` を optional にする（waiting_merge blocked では空）
- `recovery_task.text` を durable に持つ
- `auto_recover` の decision を同じ塊にぶら下げる

### 5-2. blocked を検出した時点で 1 回だけ context を確定する

blocked 検出直後に:

1. raw runner output から `BlockedContext` を作る
2. `state` / `runner-state` / event / notification はそこから派生させる

形にする。

これで:

- blocked 通知
- recovery decision 通知
- recovery halt 通知
- status 表示

が同じ source of truth を使える。

### 5-3. `state.waiting_reason` を唯一の情報源にしない

`state.waiting_reason` は今後も軽量 summary として残してよいが、以下の用途へ直接使うべきではない。

- auto-recover dedupe の元データ
- recovery task text 生成の元データ
- follow-up 通知の主データ

これらは `BlockedContext` から取る。

### 5-4. recovery task text を event / runner-state に残す

`append_recovery_task_for_blocked` の戻り値から少なくとも以下を durable 化する。

- `recovery_task.id`
- `recovery_task.line_no`
- `recovery_task.text`

そうしないと、follow-up 通知や recovery halt 通知で「実際に何を積んだか」を tasklist 再読込なしで説明できない。

### 5-5. bounded storage にする

full stdout/stderr を無制限に持つ必要はないが、少なくとも今の 1000 文字 clip よりは follow-up に使える形で残したい。

現実的には:

- `reason_detail`: 数百〜数千文字
- `runner_stdout_excerpt`: 数 KB
- `runner_stderr_excerpt`: 数 KB

程度の bounded excerpt を JSON に保存する設計が妥当。

---

## 6. 後続タスクへの具体的な受け渡し

### N2 でやるべきこと

- `BlockedContext` 相当の内部表現追加
- blocked 時に raw data から context を capture
- runner-state / status / event へ durable 化

### N3 でやるべきこと

- recovery task 文面を `state.waiting_reason` 由来から切り離す
- `BlockedContext` の summary/detail/hint から task 本文を生成する

### N4 でやるべきこと

- blocked 速報とは別に、recovery decision 通知を追加
- 通知内容は `BlockedContext + RecoveryTaskContext + AutoRecoverDecision` から組み立てる

### N5 でやるべきこと

- `*-RECOVER` 失敗時や guard hit 時に halt 通知を追加
- 「なぜ止まったか」「次に何を見るべきか」を context から再利用する

---

## 7. このメモの結論

現行フローでは、runner から得た:

- reason の全文
- stdout
- stderr
- recovery task text

が、それぞれ別の場所で早い段階に短文化・分断される。

特に問題なのは、auto-recover が `state.waiting_reason` という user-facing summary だけを入力にしている点で、これが:

- recovery task 文面の弱さ
- follow-up 通知で詳細を再利用できないこと
- halt 時に人間へ返す具体情報の欠落

の共通原因になっている。

後続実装では `BlockedContext` を導入し、blocked 検出時点の情報を 1 回 capture して、その後の通知・task 生成・halt 判定を同じ文脈から派生させるべき。
