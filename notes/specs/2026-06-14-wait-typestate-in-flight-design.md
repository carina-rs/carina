# wait-aware in-flight typestate — 設計

Issue: carina#3516 step (b)  
親 issue: #3497 / #3515

## 解こうとしている問題

#3515 で入れた fail-fast 機構は次の 4 ステップを **全 executor loop で順番どおりに** 実行する前提で成り立っている。

1. dispatch するときに、wait なら `tokio::sync::watch` channel を作って `wait_cancellers.insert(idx, sender)` し、`in_flight_kinds.insert(idx, InFlightKind::Wait)` する。non-wait は `NonWait` を入れる。
2. dispatch 直後と 結果処理直後の 2 ヶ所で `cancel_waits_if_terminal(in_flight_kinds, undispatched, wait_cancellers)` を呼ぶ。
3. `in_flight.next().await` から返ってきたら `in_flight_kinds.remove(&idx)` と `wait_cancellers.remove(&idx)` をする。
4. wait の `SingleEffectResult` を `match` するときに、`Satisfied / Unsatisfiable / Timeout / NotFound / ReadFailed` を漏れなく扱う(これは `WaitOutcome` の型で保証されている)。

このうち 1〜3 は **生 `HashMap` と `FuturesUnordered` の直叩き**で書かれていて、新しい executor loop を足すときに「うっかり cancel 呼び忘れる」「`wait_cancellers` の insert を忘れる」「remove を忘れる」が全部 compile error にならない。#3515 の review round 1 で指摘されたとおり、これは convention level の保護。

`InFlightKind::Wait` ⇄ `wait_cancellers` の 2 つを別々のマップに散らかしているのもエラーを誘発する形。同じ idx に対して片方だけ insert / 片方だけ remove する書き間違いがしうる。

## ねらい

- **型レベル**で「wait future の push」と「cancel sender の registration」を 1 アクションにまとめる。
- **型レベル**で「`in_flight.next().await` の前に terminal check を必ず通る」ことを強制する。
- 既存の 2 ヶ所 (`parallel.rs::execute_effects_sequential` と `phased.rs::execute_effects_phased` Phase 1) の loop を、新型を使う形に書き直す。挙動と test は何も変えない。
- `unsatisfiable_reason_message` / `wait_failure_message` / `InFlightKind` は既に wait.rs に集約済み。これらは触らない。

## 型シェイプ

`carina-core/src/executor/wait.rs` に追加する `WaitAwareInFlight` 構造体。

```rust
pub(super) struct WaitAwareInFlight<'a, R> {
    inner: FuturesUnordered<Pin<Box<dyn Future<Output = (usize, R)> + 'a>>>,
    kinds: HashMap<usize, InFlightKind>,
    cancellers: HashMap<usize, watch::Sender<bool>>,
}

impl<'a, R> WaitAwareInFlight<'a, R> {
    pub fn new() -> Self { ... }

    /// dispatch a non-wait future. Caller hands over a future producing
    /// `(idx, R)`. Tracking is automatic.
    pub fn push_non_wait(
        &mut self,
        idx: usize,
        fut: impl Future<Output = (usize, R)> + 'a,
    ) { ... }

    /// dispatch a wait future. Returns the cancel `watch::Receiver` so the
    /// caller threads it into `execute_wait_effect`. The sender is owned
    /// internally — caller cannot forget to register it.
    pub fn push_wait(
        &mut self,
        idx: usize,
        fut_with_cancel: impl FnOnce(watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = (usize, R)> + 'a>>,
    ) { ... }

    /// Returns the typestate-guarded ready handle.
    /// Caller MUST consume it before the next dispatch tick.
    /// Holds the loop progress fact ("we just observed terminal-or-not").
    #[must_use = "TerminalCheck must be consumed via inspect_terminal() to advance the loop"]
    pub fn check_terminal(
        &mut self,
        undispatched_count: usize,
    ) -> TerminalCheck<'_, 'a, R> { ... }

    pub fn is_empty(&self) -> bool { self.inner.is_empty() }

    pub fn len(&self) -> usize { self.inner.len() }
}

/// One-shot RAII handle: created by `check_terminal()`, consumed by
/// `next_completed()`. While alive, no further push is allowed
/// (compile-enforced by &mut borrow of `WaitAwareInFlight`).
#[must_use]
pub(super) struct TerminalCheck<'a, 'fut, R> {
    parent: &'a mut WaitAwareInFlight<'fut, R>,
}

impl<'a, 'fut, R> TerminalCheck<'a, 'fut, R> {
    /// If the in-flight set is exactly "only waits, nothing else to
    /// dispatch", send cancel to every wait. Idempotent.
    pub fn cancel_if_terminal(self) -> NextReady<'a, 'fut, R> {
        if cancel_waits_if_terminal(&self.parent.kinds, /* undispatched */ 0, &self.parent.cancellers) {
            // signal sent
        }
        NextReady { parent: self.parent }
    }
}

#[must_use]
pub(super) struct NextReady<'a, 'fut, R> {
    parent: &'a mut WaitAwareInFlight<'fut, R>,
}

impl<'a, 'fut, R> NextReady<'a, 'fut, R> {
    /// Await the next completion. On completion, cleans up `kinds` and
    /// `cancellers` for that idx — caller cannot forget.
    pub async fn next_completed(self) -> Option<(usize, R)> {
        let (idx, r) = self.parent.inner.next().await?;
        self.parent.kinds.remove(&idx);
        self.parent.cancellers.remove(&idx);
        Some((idx, r))
    }
}
```

### この型で禁止される書き方

- `in_flight.push(...)` を直接呼ぶ → 型上できない(`inner` は `pub` でない)。
- wait future を push して cancel sender の登録を忘れる → `push_wait` の引数が `FnOnce(Receiver) -> Future` なので、cancel channel を作って sender を登録した上で receiver を渡す経路しか書けない。
- `check_terminal()` を呼ばずに `next().await` する → `next_completed` は `NextReady` の method、`NextReady` は `TerminalCheck::cancel_if_terminal()` 経由でしか作れない。`#[must_use]` も付くので drop で warning。
- `next().await` 後の `remove` 忘れ → `NextReady::next_completed` の中で必ず行うので呼び忘れ不能。

両方の loop が `check_terminal().cancel_if_terminal().next_completed()` のチェーンを通る形になる。新しい loop を将来追加する人は、push しただけでは next を await できず、自然に terminal check 経由のチェーンを書く。

### `cancel_waits_if_terminal` の引数の取り回し

現状の helper は `undispatched_count: usize` を引数で取る。`TerminalCheck` から呼ぶときも `undispatched_count` が必要なので、`check_terminal(undispatched_count)` が引数を受け取り `TerminalCheck` がそれを保持して `cancel_if_terminal` 内で使う。

### dispatch loop での 2 回チェック

#3515 では「dispatch loop の後」と「結果処理の後」の 2 ヶ所で `cancel_waits_if_terminal` を呼んでいた。新型でも同じ 2 ヶ所で `check_terminal(...).cancel_if_terminal()` を呼べばよい(2 回目の戻り値 `NextReady` を `next_completed` で消費)。1 回目の戻り値 `NextReady` は使わずに drop しても `#[must_use]` の warning が出るので分かる。

1 回目の呼び出しが「過渡的(まだ next_completed の前)」、2 回目が「実際に進む」という形にする。両方とも `cancel_if_terminal` までは呼んで、1 回目だけ `NextReady::next_completed` を呼ばずに済む API にしたい。具体的には:

```rust
// 1 回目: dispatch 直後。状態だけ更新したい。
in_flight.check_terminal(undispatched()).cancel_if_terminal().drop_without_awaiting();

// 2 回目: 実際に next 取り出す。
let (idx, r) = in_flight
    .check_terminal(undispatched())
    .cancel_if_terminal()
    .next_completed()
    .await
    .expect("in_flight non-empty in loop body");
```

`drop_without_awaiting()` は `NextReady` を消費して `()` を返すだけのメソッド。`#[must_use]` を満たす逃げ道。

## 範囲

- `carina-core/src/executor/wait.rs` に typestate 型 (`WaitAwareInFlight`, `TerminalCheck`, `NextReady`) を追加。既存 helper (`cancel_waits_if_terminal` 等) は内部で再利用。
- `carina-core/src/executor/parallel.rs::execute_effects_sequential` を新型に書き換え。生 `in_flight: FuturesUnordered`、`in_flight_kinds: HashMap`、`wait_cancellers: HashMap` を削除し、`WaitAwareInFlight` に置換。
- `carina-core/src/executor/phased.rs::execute_effects_phased` Phase 1 で同じ置換。Phase 2/3/4 は wait を扱わないので非対象。
- 既存 test は全部素通り(挙動変更なし)。新規 test は不要 — 既存の `wait_marked_unsatisfiable_when_only_waits_in_flight` 系が依然として通れば、挙動互換は証明された。

## 1 PR でやり、やらないこと

やる: 上記 4 つ。  
やらない: step (c) の plan-time mutator inference。

## 検証戦略

- 既存 test 一式 (parallel/phased の `wait_marked_unsatisfiable_*` を含む) が変更なしで全部 pass。
- `cargo nextest run --workspace --all-features` / clippy / doc-tests / scripts/check-*.sh が全部 green。
- 静的に意図された compile error が起きることを確かめる "trybuild" 風テストは入れない(workspace に trybuild が無いため範囲外)。代わりに、新しい API doc に「これらを直接 push したり remove したりできない」旨を書いて signal にする。

## 段階

1. 設計レビュー(この doc)
2. 型を追加(wait.rs)
3. parallel.rs 置換
4. phased.rs 置換
5. verify → 5-round review → PR
