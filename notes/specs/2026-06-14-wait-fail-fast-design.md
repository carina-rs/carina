# wait fail-fast on unsatisfiable conditions — 設計

Issue: carina#3497
ブランチ: `issue-3497-wait-fail-fast`

## 何を解決するか

`wait` ブロックの `until` 条件が、他のリソース失敗の結果として
**もはや真にならないことが確定した** 時点で、ポーリングを打ち切って
`Skipped(Unsatisfiable)` として終了させる。現状は `timeout`
(75min など) まで silently poll し続ける。

副次対応として、ポーリング中の **観測性向上** のため heartbeat
イベントを出す。

## 根本原因

`Effect::Wait` の executor は次の前提で動いている。

1. wait の依存(`target` と `explicit_dependencies`)が **すべて完了
   または失敗**するまで dispatch を保留する
2. dispatch 後はひたすら `until` を poll する
3. `until` が真にならないまま `timeout` を超えたら `ProviderError::Timeout`

しかし `wait` の `until` を真にする経路は **plan 時に静的に決まらない**:

- `until: cert.status == issued` は `cert` の attribute を読むが、
  この属性を変化させるのは **別のリソース** (この issue の例では
  `for opt in cert.domain_validation_options` から展開される
  `route53.RecordSet` 群) であり、これらは plan 時点で deferred 展開
  状態のまま `?` 行として表示される
- ユーザは `depends_on = [validation_records]` で正しく明示している
  が、`validation_records` 自体が **deferred-for binding** であり、
  apply pre-refresh 時点で `cert.domain_validation_options` がまだ
  unknown なので **展開されないまま** apply ループに入る
- 加えて、deferred-for 展開を駆動する `cert` 周辺のリソース
  (ALB, ListenerなどLB系) が IAM 不足等で失敗した場合、validation
  records 経路はもはや実行されない

executor の **「他に dispatch すべき effect が無い、in_flight には wait
しか残っていない」** 状態は、wait の `until` を真にする世界の変化が
もう起こらないことを意味する。が、現状の loop はこの状態を
unsatisfiable と認識せず、 wait の poll を `timeout` まで放置する。

## 設計の核

executor の loop に **terminal-with-pending-waits** 検出を追加し、
その状態に到達した瞬間に in_flight 中の wait に **cancel シグナル**
を送る。wait は cancel シグナルを受けて `WaitOutcome::Unsatisfiable`
を返し、observer に `EffectSkipped` を `reason: "unsatisfiable: …"`
で emit する。

### 型レベルの不変条件

`execute_wait_effect` の戻り値を整理する。現在は
`ProviderResult<State>`(`Ok(State) | Err(ProviderError)`)で、
unsatisfiable を表せず Timeout に押し込めるしかない。

```rust
pub enum WaitOutcome {
    Satisfied(State),
    Unsatisfiable(UnsatisfiableReason),  // executor が cancel を送った
    Timeout { last_attrs: HashMap<String, Value>, elapsed: Duration },
    NotFound(ProviderError),
    ReadFailed(ProviderError),
}

pub enum UnsatisfiableReason {
    /// 静的に判定された: dispatch する前に depends_on のどれかが failed
    DependencyFailed { binding: String },
    /// 動的に判定された: in_flight が wait のみになった
    NoMutatorRemaining,
}
```

呼び出し側は `match outcome` で網羅的に処理する。`_ =>` で
Unsatisfiable を漏らすことが型として不可能になる。

`SingleEffectResult::Wait` も同様に変える。現在の
`{ success: bool, … }` という bool 表現は Satisfied / Unsatisfiable /
Timeout を区別できず、observer が表示すべきメッセージを呼び出し側で
組み立てないといけない。これを `outcome: WaitOutcome` に置き換える。

### Cancel の伝播

`execute_wait_effect` は `CancellationToken` を引数で受ける。poll loop
は `tokio::select!` で「次の interval」「cancel」を同時に待つ。
cancel が来たら `Unsatisfiable(NoMutatorRemaining)` を返す。

executor 側は `in_flight` に push する各 wait 用に
`CancellationToken` を持ち、loop の各 iteration で:

```text
1. newly_ready を計算
2. dispatch
3. in_flight.next() を await して 1 件処理
4. terminal-with-pending-waits 判定:
   - newly_ready が空(計算済み)
   - actionable_indices のうち未 dispatch が 0
   - in_flight が空でない、かつ in_flight の全要素が Wait
   なら、それら wait の cancellation_token を cancel
5. 通常通り次の loop へ
```

判定 4 は **アイドルな wait しか残っていない** 状態。これを発見した
時点で速やかに cancel シグナルが伝播する。wait 側は次の `select!`
で cancel を観測し `Unsatisfiable(NoMutatorRemaining)` で返す。

### Heartbeat イベント

`execute_wait_effect` の poll loop に observer 呼び出しを 1 つ足す:

```rust
observer.on_event(&ExecutionEvent::WaitPolling {
    effect,
    elapsed,
    last_attrs: &state.attributes,
});
```

これだけだと **すべての poll で出してうるさい** ので、最後の emit
からの経過時間が `max(30s, interval * 5)` を超えたときだけ出す。
interval が 5s ならおおむね 30s 間隔、interval が 60s なら
5min 間隔。

`ExecutionEvent::WaitPolling` を `ExecutionEvent` に追加し、CLI 表示
側で「`~ N秒経過 (cert.status = pending_validation)`」のような行を
1 行だけ出す(改行はせず、上書き or 短く)。

### Dispatch 前の depends_on 失敗チェック

これは既存の `find_failed_dependency` で動作している(Phase 1 の
404 行, Phase 4 の 1052 行, parallel の 599 行)。`depends_on` に書か
れた binding が `failed_bindings` に入っていれば dispatch 前に
EffectSkipped で完了する。**この経路は触らない**。

ただし `find_failed_dependency` が Wait の `explicit_dependencies` を
ちゃんと見ているか確認する。`deps.rs:312` の実装次第。見ていなければ
そこは修正(これは別の根本的な seam なので、この PR の射程内)。

## 何が unrepresentable になるか

- `execute_wait_effect` の戻り値が **bool ではなく typed enum** に
  なり、「Satisfied と Unsatisfiable と Timeout を取り違える」
  バグはコンパイル時に消える
- executor の loop が **terminal-with-pending-waits** という状態を
  陽に持つので、「他に動くものが無くなった瞬間に wait を放置」
  という silent hang はコードパスとして書けなくなる
- `SingleEffectResult::Wait { success: bool, … }` の bool 表現が
  消える(後方互換は無視してよいプロジェクト方針)

## 何を 1 PR でやり、何をやらないか

1 PR でやる:

1. `WaitOutcome` enum 導入と `execute_wait_effect` のシグネチャ変更
2. `CancellationToken` を `execute_wait_effect` に渡す
3. executor の loop に terminal-with-pending-waits 判定を追加
4. `SingleEffectResult::Wait` を `WaitOutcome` を持つ形に変更
5. observer に `EffectSkipped(reason="unsatisfiable: …")` を emit
6. `ExecutionEvent::WaitPolling` 追加と CLI 表示の heartbeat
7. reproducing test:
   - in_flight の最後の非 wait effect が失敗 → wait は即 Skip
   - 通常 satisfied → 既存挙動を保つ
   - 通常 timeout → 既存挙動を保つ
   - heartbeat イベントが期待間隔で出ること

1 PR ではやらない:

- `wait` の `until` 表現を拡張して mutator 集合を静的解析する話
  (= Issue 本文の選択肢 #1 の精密版)。これは別 issue。
  今回の terminal-with-pending-waits 判定は、その将来の精密化が
  入っても **deep-defense として有効**な不変条件。
- deferred-for 展開のタイミング再設計
  (apply ループ中に再展開を試みる)。これは別 issue。

## テスト戦略

1. **単体 (executor)**: in-flight に「成功する create 1 件」と
   「指定 binding を mutate する wait 1 件」を投げて、create が
   失敗してから 1 interval 以内に wait が `Unsatisfiable` で
   終わることを assert。Issue の構造を最小化したもの。

2. **単体 (wait.rs)**: `CancellationToken` を `cancel()` した時に
   `Unsatisfiable(NoMutatorRemaining)` が返ることを assert。

3. **単体 (wait.rs)**: 既存テスト 4 件
   (`wait_returns_immediately_when_until_already_true` /
    `wait_polls_until_predicate_becomes_true` /
    `wait_returns_timeout_when_predicate_stays_false` /
    `wait_returns_not_found_when_target_disappears`)
   をすべて `WaitOutcome` で書き直し、戻り値の match が網羅される
   ことを確認。

4. **observer**: heartbeat が `max(30s, interval*5)` 間隔で出ることを
   mock observer で assert。短い timeout でテストしやすいよう
   interval を 1ms に設定したケースでは heartbeat は 1 回だけ
   (1ms * 5 = 5ms 間隔)。

5. **E2E (carina-cli)**: 既存の wait 関連の integration test (あれば)
   に「上流リソース失敗 → wait 即 fail」のシナリオを追加。

## 段階

1. 設計レビュー(この doc を読む)
2. 失敗する test を書く
3. `WaitOutcome` 型を入れる
4. executor の terminal 判定を入れる
5. heartbeat を入れる
6. observer / CLI 表示を更新
7. verify → simplify → 5 round review → PR
