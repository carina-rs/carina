# Apply interruption resilience TDD plan
<!-- derived-from ../specs/2026-06-14-apply-interruption-resilience-design.md -->

対応 issue: https://github.com/carina-rs/carina/issues/3498
設計仕様書: `docs/specs/2026-06-14-apply-interruption-resilience-design.md`
Codex 実装担当者はこの plan の順番どおりに Red -> Green -> Refactor を 1 タスクずつ進める。
この plan は実装作業を含まない。実装時は state 永続化、lock release、provider in-flight 待機の順序を崩さない。

## T1. CancellationToken 依存の選定

依存タスク: なし

**Goal**: `carina-core` と `carina-cli` の両方で同じ cancel token 型を使えるようにする。

**Files**:

- modify `carina-core/Cargo.toml`
- modify `carina-cli/Cargo.toml`

**Test**:

このタスクでは Rust test は追加しない。依存追加後の compile check を Red/Green の確認にする。

**Implementation**:

`tokio-util::sync::CancellationToken` を採用する。`tokio-util` は既に `Cargo.lock` に存在し、workspace へ新しい crate family を増やさない。API は `clone()`, `cancel()`, `is_cancelled()`, `cancelled().await` を持ち、executor の loop と confirm prompt の `tokio::select!` の両方にそのまま使える。自前 token は `Future` 化、wake 管理、clone 間共有、二重 cancel の順序を実装する必要があり、今回の修正範囲より大きい。`carina-core` は executor の public signature に token を出すため直接依存、`carina-cli` は signal listener と prompt で token を生成・待機するため直接依存にする。

```toml
# carina-core/Cargo.toml
[dependencies]
tokio-util = "0.7"
```

```toml
# carina-cli/Cargo.toml
[dependencies]
tokio-util = "0.7"
```

**Verify**:

```bash
cargo check -p carina-core
cargo check -p carina-cli
```

## T2. ExecutionOutcome enum を追加する

依存タスク: T1

**Goal**: executor の結果が通常完了か cancel 完了かを型で表現できる。

**Files**:

- modify `carina-core/src/executor/mod.rs`
- modify `carina-core/src/executor/tests.rs`

**Test**:

```rust
#[test]
fn execution_outcome_completed_and_cancelled_are_matchable() {
    let completed = ExecutionOutcome::Completed(empty_execution_result());
    let cancelled = ExecutionOutcome::Cancelled(empty_execution_result());

    match completed {
        ExecutionOutcome::Completed(result) => {
            assert_eq!(result.success_count, 0);
            assert!(result.applied_states.is_empty());
        }
        ExecutionOutcome::Cancelled(_) => panic!("completed outcome changed variant"),
    }

    match cancelled {
        ExecutionOutcome::Cancelled(result) => {
            assert_eq!(result.failure_count, 0);
            assert!(result.successfully_deleted.is_empty());
        }
        ExecutionOutcome::Completed(_) => panic!("cancelled outcome changed variant"),
    }
}
```

型レベルの negative property は test note として残す。`ExecutionOutcome` から `Result<ExecutionResult, _>` への `From` / `Into` impl は作らない。`execute_plan(...).await?` が compile しない状態を設計上の証拠にする。

```rust
/// ```compile_fail
/// use carina_core::executor::{execute_plan, ExecutionResult};
///
/// async fn cannot_question_mark_execution_outcome(
///     provider: &dyn carina_core::provider::Provider,
///     input: carina_core::executor::ExecutionInput<'_>,
///     observer: &dyn carina_core::executor::ExecutionObserver,
///     cancel: tokio_util::sync::CancellationToken,
/// ) -> Result<ExecutionResult, String> {
///     let result = execute_plan(provider, input, observer, cancel).await?;
///     Ok(result)
/// }
/// ```
pub struct ExecutionOutcomeCannotBeQuestionMarked;
```

**Implementation**:

`carina-core::executor` に仕様書の enum を追加する。accessor は追加しない。caller が `match` で両 variant を明示する形を固定する。`From<ExecutionOutcome> for Result<ExecutionResult, _>` と `Try` 相当の逃げ道は作らない。

```rust
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Cancelled(ExecutionResult),
}
```

test 用に `empty_execution_result()` を `carina-core/src/executor/tests.rs` の既存 test module 内へ置く。

**Verify**:

```bash
cargo nextest run -p carina-core execution_outcome_completed_and_cancelled_are_matchable
cargo test -p carina-core --doc ExecutionOutcomeCannotBeQuestionMarked
```

## T3. execute_plan の signature を変更する

依存タスク: T2

**Goal**: cancel token 引数を受け取るが、まだ監視せず通常経路では `ExecutionOutcome::Completed` を返す。

**Files**:

- modify `carina-core/src/executor/mod.rs`
- modify `carina-core/src/executor/tests.rs`
- modify `carina-core/tests/wait_downstream_apply.rs`
- modify `carina-cli/tests/wait_apply_module_path.rs`
- modify `carina-cli/src/commands/apply/mod.rs`

**Test**:

```rust
#[tokio::test]
async fn execute_plan_returns_completed_when_not_cancelled() {
    let provider = MockProvider::default();
    let observer = NoopObserver;
    let mut state_file = StateFile::default();
    let plan = create_single_resource_plan("test", "one");
    let input = ExecutionInput::new_mut_state(&plan, &mut state_file);
    let cancel = CancellationToken::new();

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    match outcome {
        ExecutionOutcome::Completed(result) => {
            assert_eq!(result.failure_count, 0);
            assert_eq!(result.success_count, 1);
        }
        ExecutionOutcome::Cancelled(_) => panic!("uncancelled execution returned Cancelled"),
    }
}
```

**Implementation**:

`execute_plan` の signature を仕様書どおりに変える。`cancel` は `_cancel` として受け取り、phased / sequential の戻り値を `Completed` に包む。

```rust
pub async fn execute_plan(
    provider: &dyn Provider,
    mut input: ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    _cancel: CancellationToken,
) -> ExecutionOutcome {
    let result = if has_interdependent_replaces(input.plan.effects()) {
        execute_effects_phased(provider, &mut input, observer).await
    } else {
        execute_effects_sequential(provider, &mut input, observer).await
    };
    ExecutionOutcome::Completed(result)
}
```

既存の `execute_plan(&provider, input, &observer).await` 呼び出しは `CancellationToken::new()` を渡す形に直す。T3 は signature 変更で workspace を壊したまま終わらせない。

caller 追従対象:

- `carina-core/src/executor/tests.rs`: `execute_plan(` を呼ぶ全 test
- `carina-core/tests/wait_downstream_apply.rs`: line 284 と line 431 の 2 箇所
- `carina-cli/tests/wait_apply_module_path.rs`: line 463 の 1 箇所
- `carina-cli/src/commands/apply/mod.rs`: `execute_effects` wrapper 内の `carina_core::executor::execute_plan(provider, input, &observer).await`

T3 の caller 追従は token を伝播しない。各 caller の局所修正は `CancellationToken::new()` を渡すだけにする。

**Verify**:

```bash
cargo nextest run -p carina-core execute_plan_returns_completed_when_not_cancelled
cargo check --workspace
```

## T4. executor loop で cancel token を監視する

依存タスク: T3

**Goal**: cancel 後は新しい effect を投入せず、in-flight effect は完了まで待って `Cancelled(result)` に含める。

**Files**:

- modify `carina-core/src/executor/mod.rs`
- modify `carina-core/src/executor/parallel.rs`
- modify `carina-core/src/executor/phased.rs`
- modify `carina-core/src/executor/tests.rs`

**Test**:

```rust
#[tokio::test(start_paused = true)]
async fn execute_plan_cancelled_after_three_completed_keeps_in_flight_and_drops_pending() {
    let provider = DelayedCountingProvider::new(Duration::from_secs(10));
    let observer = CancelsAfterSuccesses::new(3);
    let cancel = observer.token();
    let mut state_file = StateFile::default();
    let plan = create_independent_create_plan(["r1", "r2", "r3", "r4", "r5"]);
    let input = ExecutionInput::new_mut_state(&plan, &mut state_file)
        .with_parallelism(NonZeroUsize::new(1).unwrap());

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled run returned Completed"),
    };
    assert_eq!(result.applied_states.len(), 3);
    assert_eq!(provider.started_names(), vec!["r1", "r2", "r3"]);
}
```

```rust
#[tokio::test(start_paused = true)]
async fn execute_plan_cancelled_while_effect_in_flight_records_that_effect() {
    let provider = BlockingProvider::new("r2", Duration::from_secs(60));
    let observer = CancelsWhenStarted::new("r2");
    let cancel = observer.token();
    let mut state_file = StateFile::default();
    let plan = create_independent_create_plan(["r1", "r2", "r3"]);
    let input = ExecutionInput::new_mut_state(&plan, &mut state_file)
        .with_parallelism(NonZeroUsize::new(1).unwrap());

    let task = tokio::spawn(async move { execute_plan(&provider, input, &observer, cancel).await });
    tokio::time::advance(Duration::from_secs(60)).await;
    let outcome = task.await.unwrap();

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled run returned Completed"),
    };
    assert!(result.applied_states.contains_key(&ResourceId::new("test", "r2")));
}
```

```rust
#[tokio::test(start_paused = true)]
async fn execute_plan_phased_cancelled_between_phases_keeps_completed_resources_in_state() {
    let provider = CancellingReplaceProvider::new(Duration::from_secs(5));
    let observer = CancelsAfterReplacePhase::new(ReplacePhase::CreateTemporary);
    let cancel = observer.token();
    let mut state_file = StateFile::default();
    let plan = create_interdependent_replace_plan();
    assert!(has_interdependent_replaces(plan.effects()));
    let input = ExecutionInput::new_mut_state(&plan, &mut state_file)
        .with_parallelism(NonZeroUsize::new(2).unwrap());

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("phased cancel returned Completed"),
    };
    assert!(result.applied_states.contains_key(&ResourceId::new("test", "a")));
    assert!(!provider.started_phase(ReplacePhase::DeleteOld));
}
```

**Implementation**:

`execute_effects_sequential` の戻り値を `ExecutionOutcome` に変え、loop の dispatch 前で `cancel.is_cancelled()` を見て `newly_ready.clear()` する。`in_flight` が空になった時点で `cancelled` flag が true なら `ExecutionOutcome::Cancelled(result)`、false なら `Completed(result)` を返す。

```rust
let mut cancelled = false;
loop {
    if cancel.is_cancelled() {
        cancelled = true;
    }
    if !cancelled {
        dispatch_newly_ready_effects(...);
    }
    if in_flight.is_empty() {
        break;
    }
    let (finished_idx, result) = in_flight.next().await.unwrap();
    process_single_effect_result(...);
}
let result = build_execution_result(...).await;
if cancelled {
    ExecutionOutcome::Cancelled(result)
} else {
    ExecutionOutcome::Completed(result)
}
```

`phased.rs` は T4 で `cancel` を受け取る signature に揃え、phase 1 の create temporary が終わった後、phase 2 の delete old を投入する前に `cancel.is_cancelled()` を見る。`create_interdependent_replace_plan()` は `has_interdependent_replaces(plan.effects()) == true` になる Replace effect を 2 件作る helper として `carina-core/src/executor/tests.rs` に置く。`parallel.rs` の loop は effect 単位の cancel semantics を担う。

**Verify**:

```bash
cargo nextest run -p carina-core execute_plan_cancelled_after_three_completed_keeps_in_flight_and_drops_pending
cargo nextest run -p carina-core execute_plan_cancelled_while_effect_in_flight_records_that_effect
cargo nextest run -p carina-core execute_plan_phased_cancelled_between_phases_keeps_completed_resources_in_state
```

## T5. apply.rs で ExecutionOutcome を match し Cancelled でも finalize_apply を走らせる

依存タスク: T4

**Goal**: apply 中の cancel で完了済み state を保存し、lock を release し、最後に `AppError::Interrupted` を返す。

**Files**:

- modify `carina-cli/src/commands/apply/mod.rs`
- modify `carina-cli/src/commands/apply/tests.rs`
- create `carina-cli/src/commands/apply/tests/cancellation_fixture.rs`

**Test**:

```rust
#[tokio::test]
async fn run_apply_cancelled_after_partial_execution_persists_state_and_releases_lock() {
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["first", "second", "third"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let err = run_apply(
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        fixture.provider_context(),
        token,
    )
    .await
    .unwrap_err();

assert!(matches!(err, AppError::Interrupted));
    let state = fixture.backend().read_state().await.unwrap().unwrap().into_state();
    assert!(state.resource(ResourceId::new("mock", "first")).is_some());
    assert!(state.resource(ResourceId::new("mock", "second")).is_none());
    assert!(!fixture.backend().lock_path().exists());
}
```

**Implementation**:

`run_apply`, `run_apply_locked`, `run_apply_from_plan`, `run_apply_from_plan_locked`, `execute_effects` に `CancellationToken` を通す。`execute_effects` は `ExecutionOutcome` を返す。`run_apply_locked` は outcome を match して `finalize_apply` に `&result` を渡し、finalize 完了後に cancelled なら `Err(AppError::Interrupted)` を返す。

fixture interface:

- `ApplyCancellationFixture::new() -> Self`
- `with_resources<const N: usize>(self, names: [&str; N]) -> Self`
- `cancel_after_successes(self, count: usize) -> Self`
- `cancel_token(&self) -> CancellationToken`
- `read_state(&self) -> impl Future<Output = StateFile>`
- `lock_path(&self) -> &Path`
- `provider_context(&self) -> &ProviderContext`
- `config_path(&self) -> &Path`
- `backend(&self) -> &LocalStateBackendForTest`

`ResourceId::new("mock", name)` は `carina-provider-mock` の `ProviderInfo { name: "mock", ... }` に合わせる。fixture が生成する `.crn` には `provider "mock" {}` と `resource "mock.<type>" "<name>" {}` を含め、`ProviderContext` には mock provider factory を登録する。

```rust
let outcome = execute_effects(..., cancel.clone()).await;
let (mut result, cancelled) = match outcome {
    ExecutionOutcome::Completed(result) => (result, false),
    ExecutionOutcome::Cancelled(result) => (result, true),
};
execute_import_effects(&plan, &provider, &mut result).await;
execute_state_only_effects(&plan, &mut result);
finalize_apply(FinalizeApplyInput { result: &result, ... }).await?;
if cancelled {
    return Err(AppError::Interrupted);
}
```

`run_apply` の lock release は `op_result` が `Interrupted` でも release する既存 pattern を維持する。T5 では saved-plan apply にも同じ token を渡す。

**Verify**:

```bash
cargo nextest run -p carina-cli run_apply_cancelled_after_partial_execution_persists_state_and_releases_lock
```

## T6. destroy.rs の独自 loop を cancel token 対応にし state 書き戻しを走らせる

依存タスク: T5

**Goal**: destroy 中の cancel で削除済み resource を state から反映し、lock を release し、最後に `AppError::Interrupted` を返す。

**Files**:

- modify `carina-cli/src/commands/destroy.rs`
- modify `carina-cli/src/tests.rs`
- create `carina-cli/src/commands/destroy/tests/cancellation_fixture.rs`

**Test**:

```rust
#[tokio::test]
async fn run_destroy_cancelled_after_partial_execution_persists_deletions_and_releases_lock() {
    let fixture = DestroyCancellationFixture::new()
        .with_existing_resources(["first", "second"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let err = run_destroy(
        fixture.config_path(),
        true,
        true,
        false,
        false,
        NonZeroUsize::new(1).unwrap(),
        fixture.provider_context(),
        token,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.backend().read_state().await.unwrap().unwrap().into_state();
    assert!(state.resource(ResourceId::new("mock", "first")).is_none());
    assert!(state.resource(ResourceId::new("mock", "second")).is_some());
    assert!(!fixture.backend().lock_path().exists());
}
```

**Implementation**:

`run_destroy` と `run_destroy_locked` に `CancellationToken` を追加する。`destroy.rs` には現存する state writeback helper が無く、`run_destroy_locked` 末尾で `apply_destroy_to_state(&mut state, &destroyed_ids)` と `save_state_locked` / `save_state_unlocked` を直接呼んでいる。T6 で新規 `finalize_destroy` helper を追加し、cancelled でもその helper を必ず通る形にする。

fixture interface:

- `DestroyCancellationFixture::new() -> Self`
- `with_existing_resources<const N: usize>(self, names: [&str; N]) -> Self`
- `cancel_after_successes(self, count: usize) -> Self`
- `cancel_token(&self) -> CancellationToken`
- `read_state(&self) -> impl Future<Output = StateFile>`
- `lock_path(&self) -> &Path`
- `provider_context(&self) -> &ProviderContext`
- `config_path(&self) -> &Path`
- `backend(&self) -> &LocalStateBackendForTest`

`ResourceId::new("mock", name)` は `carina-provider-mock` の provider name に合わせる。fixture は initial state に mock resource を入れ、`.crn` 側にも同じ mock provider configuration を生成する。

```rust
let cancelled = cancel.is_cancelled();
finalize_destroy(FinalizeDestroyInput {
    backend,
    lock,
    state_file,
    destroyed_ids: &destroyed_ids,
    success_count,
    failure_count,
    skip_count,
}).await?;
if cancelled {
    return Err(AppError::Interrupted);
}
```

`run_destroy` の outer lock release は T5 と同じく `Err(AppError::Interrupted)` を release 対象として扱う。

**Verify**:

```bash
cargo nextest run -p carina-cli run_destroy_cancelled_after_partial_execution_persists_deletions_and_releases_lock
```

## T7. signal.rs を CancellationToken 駆動に書き換える

依存タスク: T6

**Goal**: SIGINT と SIGTERM の初回で token を cancel し、2 回目で cursor restore 後に exit 130 する統一 listener を持つ。

**Files**:

- modify `carina-cli/src/signal.rs`

**Test**:

```rust
#[tokio::test]
async fn signal_listener_cancels_token_on_interrupt_event() {
    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let exit = RecordingExit::default();

    let task = tokio::spawn(listen_for_shutdown_events(
        token.clone(),
        SignalEvents::from_receiver(rx),
        exit.clone(),
    ));
    tx.send(ShutdownSignal::Interrupt).unwrap();

    token.cancelled().await;
    assert!(!exit.was_called());
    task.abort();
}
```

```rust
#[tokio::test]
async fn signal_listener_cancels_token_on_terminate_event() {
    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let exit = RecordingExit::default();

    tokio::spawn(listen_for_shutdown_events(
        token.clone(),
        SignalEvents::from_receiver(rx),
        exit.clone(),
    ));
    tx.send(ShutdownSignal::Terminate).unwrap();

    token.cancelled().await;
    assert!(!exit.was_called());
}
```

```rust
#[tokio::test]
async fn signal_listener_calls_exit_130_on_second_interrupt() {
    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let exit = RecordingExit::default();

    let task = tokio::spawn(listen_for_shutdown_events(
        token.clone(),
        SignalEvents::from_receiver(rx),
        exit.clone(),
    ));
    tx.send(ShutdownSignal::Interrupt).unwrap();
    token.cancelled().await;
    tx.send(ShutdownSignal::Interrupt).unwrap();
    task.await.unwrap();

    assert_eq!(exit.calls(), vec![130]);
}
```

**Implementation**:

`run_with_ctrl_c` を削除し、test 可能な内部関数を置く。production は `tokio::signal::unix::signal(SignalKind::interrupt())` と `SignalKind::terminate()` から `ShutdownSignal` を流す。

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownSignal {
    Interrupt,
    Terminate,
}

pub fn spawn_shutdown_listener(token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let events = SignalEvents::unix().expect("install unix signal handlers");
        listen_for_shutdown_events(token, events, ProcessExit).await;
    })
}

async fn listen_for_shutdown_events<E, X>(token: CancellationToken, mut events: E, exit: X)
where
    E: ShutdownEvents,
    X: ExitProcess,
{
    let _first = events.recv().await;
    token.cancel();
    let _second = events.recv().await;
    crate::cursor::restore_cursor();
    exit.exit(130);
}
```

`read_line_with_interrupt` は T10 で扱うため T7 では残す。

**Verify**:

```bash
cargo nextest run -p carina-cli signal_listener_cancels_token_on_interrupt_event
cargo nextest run -p carina-cli signal_listener_cancels_token_on_terminate_event
cargo nextest run -p carina-cli signal_listener_calls_exit_130_on_second_interrupt
```

## T8. main.rs で CancellationToken を生成して apply/destroy に渡す

依存タスク: T7

**Goal**: CLI entrypoint が signal listener を 1 回だけ spawn し、apply / apply-from-plan / destroy が同じ token を受け取る。

**Files**:

- modify `carina-cli/src/main.rs`
- modify `carina-cli/src/commands/apply/mod.rs`
- modify `carina-cli/src/commands/destroy.rs`

**Test**:

このタスクでは main binary の Rust test は追加しない。public handler signature の compile check を Red/Green の確認にする。

**Implementation**:

`main` の `Cli::parse()` 後、`CursorGuard::stdout()` 前に token を作り、signal listener を spawn する。help/version/parse error では token も listener も作らない。

```rust
let cli = Cli::parse();
let cancel_token = CancellationToken::new();
let _shutdown_listener = carina_cli::signal::spawn_shutdown_listener(cancel_token.clone());
let _cursor_guard = carina_cli::cursor::CursorGuard::stdout();

Commands::Apply { ... } => {
    if path.extension().is_some_and(|ext| ext == "json") {
        run_apply_from_plan(&path, auto_approve, lock, parallelism, &provider_context, cancel_token.clone()).await
    } else {
        run_apply(&path, auto_approve, lock, parallelism, &provider_context, cancel_token.clone()).await
    }
}
Commands::Destroy { ... } => {
    run_destroy(&path, auto_approve, lock, refresh, force, parallelism, &provider_context, cancel_token.clone()).await
}
```

**Verify**:

```bash
cargo check -p carina-cli
```

## T9. cursor.rs の独立シグナルハンドラ登録を撤去する

依存タスク: T8

**Goal**: cursor restore の signal 経路を `signal.rs` の統一 handler に集約し、panic hook は残す。

**Files**:

- modify `carina-cli/src/cursor.rs`
- modify `carina-cli/src/main.rs`
- modify `carina-cli/src/signal.rs`

**Test**:

```rust
#[test]
fn install_panic_restore_hook_restores_hidden_cursor_once() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_cursor_hidden_for_test(true);
    install_panic_restore_hook_for_test();

    let result = std::panic::catch_unwind(|| {
        panic!("cursor restore hook test");
    });

    assert!(result.is_err());
    assert!(!is_cursor_hidden_for_test());
}
```

**Implementation**:

cursor startup hook を `install_panic_restore_hook` に絞り、SIGINT/SIGTERM 用の独立 low-level signal 登録と default-handler 再送出 loop を削除する。`signal.rs` の二回目 signal 経路で `crate::cursor::restore_cursor()` を呼ぶため、SIGINT/SIGTERM restore はそこへ集約される。

```rust
pub fn install_panic_restore_hook() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_cursor_once();
        prev(info);
    }));
}
```

`main.rs` は `carina_cli::cursor::install_panic_restore_hook();` を呼ぶ。

**Verify**:

```bash
cargo nextest run -p carina-cli install_panic_restore_hook_restores_hidden_cursor_once
```

## T10. confirm prompt を CancellationToken 駆動にする

依存タスク: T9

**Goal**: apply confirmation prompt が `CancellationToken` を待ち、`tokio::signal::ctrl_c()` を直接作らない。

**Files**:

- modify `carina-cli/src/signal.rs`
- modify `carina-cli/src/commands/apply/mod.rs`

**Test**:

```rust
#[tokio::test]
async fn read_line_until_cancelled_returns_interrupted_when_token_is_cancelled() {
    let token = CancellationToken::new();
    token.cancel();
    let reader = tokio::io::BufReader::new(NeverReady);

    let err = read_line_until_cancelled(reader, token)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
}
```

```rust
#[tokio::test]
async fn confirm_apply_returns_interrupted_when_cancel_token_fires() {
    let token = CancellationToken::new();
    token.cancel();
    let reader = tokio::io::BufReader::new(NeverReady);

    let err = confirm_apply(reader, token, false).await.unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
}
```

```rust
#[tokio::test]
async fn read_line_until_cancelled_returns_interrupted_when_cancel_fires_after_subscription() {
    let token = CancellationToken::new();
    let reader = tokio::io::BufReader::new(NeverReady);
    let waiting = tokio::spawn(read_line_until_cancelled(reader, token.clone()));

    tokio::task::yield_now().await;
    token.cancel();

    let err = waiting.await.unwrap().unwrap_err();
    assert!(matches!(err, AppError::Interrupted));
}
```

**Implementation**:

`read_line_with_interrupt<R, F>` を `read_line_until_cancelled<R>(reader, cancel: CancellationToken)` に置き換える。互換 wrapper は残さない。`confirm_apply` の generic `F` も削除して token を受ける。

```rust
pub async fn read_line_until_cancelled<R>(
    reader: R,
    cancel: CancellationToken,
) -> Result<String, AppError>
where
    R: AsyncBufRead + Unpin,
{
    tokio::pin!(reader);
    let mut buf = String::new();
    tokio::select! {
        result = reader.read_line(&mut buf) => strip_line(result, buf),
        _ = cancel.cancelled() => Err(AppError::Interrupted),
    }
}
```

`run_apply_locked` と `run_apply_from_plan_locked` の confirm 呼び出しは `confirm_apply(stdin, cancel.clone(), auto_approve).await?` に変える。

**Verify**:

```bash
cargo nextest run -p carina-cli read_line_until_cancelled_returns_interrupted_when_token_is_cancelled
cargo nextest run -p carina-cli confirm_apply_returns_interrupted_when_cancel_token_fires
cargo nextest run -p carina-cli read_line_until_cancelled_returns_interrupted_when_cancel_fires_after_subscription
```

## T11. apply の SIGINT 中断統合テスト

依存タスク: T10

**Goal**: signal module を経由せず token を直接 cancel して、apply が state 保存、lock release、Interrupted をすべて満たすことを統合テストで固定する。

**Files**:

- modify `carina-cli/src/commands/apply/tests.rs`

**Test**:

```rust
#[tokio::test]
async fn apply_cancel_token_integration_persists_completed_state_releases_lock_and_returns_interrupted() {
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["alb", "listener", "target_group"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let err = run_apply(
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        fixture.provider_context(),
        token,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.read_state().await;
    assert!(state.resource(ResourceId::new("mock", "alb")).is_some());
    assert!(state.resource(ResourceId::new("mock", "listener")).is_none());
    assert!(!fixture.lock_file_exists());
}
```

**Implementation**:

T5 の focused unit/integration helper を再利用し、test 名を issue #3498 の regression として残す。T11 は apply layer の責務だけを見るため、exit code 130 は assert しない。exit code は `signal.rs` / main error rendering の layer で扱う。

このタスクの末尾で doctest も確認する。

**Verify**:

```bash
cargo nextest run -p carina-cli apply_cancel_token_integration_persists_completed_state_releases_lock_and_returns_interrupted
cargo test --workspace --doc
```

## T12. SIGTERM の扱いを固定する

依存タスク: T11

**Goal**: SIGTERM は SIGINT と同じ token 駆動で cover され、apply/destroy の state rescue test を重複させない設計を test で示す。

**Files**:

- modify `carina-cli/src/signal.rs`

**Test**:

```rust
#[tokio::test]
async fn terminate_and_interrupt_events_share_the_same_cancel_path() {
    for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
        let token = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit = RecordingExit::default();
        let task = tokio::spawn(listen_for_shutdown_events(
            token.clone(),
            SignalEvents::from_receiver(rx),
            exit.clone(),
        ));

        tx.send(signal).unwrap();
        token.cancelled().await;

        assert!(token.is_cancelled());
        assert!(!exit.was_called());
        task.abort();
    }
}
```

**Implementation**:

`SignalEvents::unix()` で `SignalKind::interrupt()` と `SignalKind::terminate()` を同じ `tokio::select!` に入れ、どちらも `ShutdownSignal` に変換する。

```rust
tokio::select! {
    _ = interrupt.recv() => Some(ShutdownSignal::Interrupt),
    _ = terminate.recv() => Some(ShutdownSignal::Terminate),
}
```

SIGTERM の apply 挙動は T11 で token fire 経路を cover している。OS signal 経由の apply test は CI の signal timing に依存するため追加しない。

**Verify**:

```bash
cargo nextest run -p carina-cli terminate_and_interrupt_events_share_the_same_cancel_path
```

## T13. 全 verify 一括

依存タスク: T12

**Goal**: workspace 全体で test、doctest、clippy、repository scripts が通ることを確認する。

**Files**:

- modify なし

**Test**:

このタスクでは Rust test は追加しない。T1 から T12 の tests と workspace checks をまとめて実行する。

**Implementation**:

実装コードは書かない。T12 までの変更がすべて入った worktree で verify だけを実行する。

**Verify**:

```bash
cargo nextest run --workspace
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
for f in scripts/check-*.sh; do bash "$f" || exit 1; done
```
