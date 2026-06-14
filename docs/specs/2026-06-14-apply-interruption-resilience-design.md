# Apply 中断時のリソース永続化とロック解放（設計）

対象 issue: [carina-rs/carina#3498](https://github.com/carina-rs/carina/issues/3498)

## 解こうとしていること

`carina apply` が途中で止められた場合に、それまでに成功したリソースが
state ファイルに残らず、backend のロックも解放されないことがある。次の
`plan` は古い state を見て「まだ何もしていない」という前提の差分を出し、
実 AWS には半分作られたリソースが孤立する。最終的にはオペレータが手作業で
リソースを片付けてロックを破る以外に復旧経路が無くなる。

issue #3498 が想定している中断のきっかけは、サブ依存リソースが落ちて apply
ループが早期に抜ける場合、ユーザーが Ctrl+C を押す場合、GitHub Actions の
step が時間切れでキャンセルされる場合、の 3 通り。前者 2 つは SIGINT、最後は
SIGTERM が引き金になる。

## 現状の挙動と何が起きているか

apply の流れは大まかに「ロックを取る → ユーザーへ確認 → `execute_plan` で
Effect を順に流す → 戻り値の `applied_states` を元に `finalize_apply` で
state ファイルを書く → ロックを解放する」の順。

state を書くのはこの 1 か所だけで、ループ中には書き込みが入らない。

中断の経路は次のようになっている。

SIGINT が来た場合は、`signal::run_with_ctrl_c` の中の `tokio::select!` が
`execute_plan` の future を drop してエラーを返す。drop された
`execute_plan` はローカルの `applied_states` HashMap を返さないので、
それまでに成功した effect の結果は呼び出し側に届かない。`finalize_apply`
は呼ばれず、state ファイルは書き換わらないまま終わる。ロックは
`run_apply` の末尾で `Interrupted` でも解放されるので残らない。

SIGTERM の場合は、`tokio::signal::ctrl_c` が拾うのは SIGINT だけなので、
SIGTERM は素通りで OS のデフォルト処理に渡され、carina プロセスは即死する。
ロックも state も何も書かれない。issue #3498 が報告している
「ロックが残ったまま」「state が古いまま」の publish-ALB のケースは、
キャンセル経由でこのパスに入った可能性が高い。

サブリソース失敗で apply ループが早期に抜けた場合は、`execute_plan` 自体は
通常 return するので `finalize_apply` まで届き、成功分の state は書ける。
ここは現状で正しく動いている。

このうち SIGTERM 裸透が一番表面的だが、本質は「`execute_plan` の戻り値が
中断時に消えるという broken invariant」が `signal` の経路で表に出ている、
の方。SIGTERM ハンドラだけ足しても、`tokio::select!` で future を drop する
seam が残る限り state は救えない。

## 採用案

`execute_plan` の戻り値の型を変えて、「中断したかどうか」を呼び出し側が
必ず判定しないとコンパイルが通らない形にする。中断のセマンティクスは
「新規 effect は投入しない、in-flight effect は完了まで待つ」に固定する。

### `ExecutionOutcome` enum

```rust
// carina-core::executor

pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Cancelled(ExecutionResult),
}

pub async fn execute_plan(
    provider: &dyn Provider,
    input: ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    cancel: CancellationToken,
) -> ExecutionOutcome
```

`Completed` と `Cancelled` のどちらも `ExecutionResult` を持つ。
`Cancelled` 側に詰める `ExecutionResult` には、cancel 通知が来るまでに
完了済みだった effect の結果（成功分の `applied_states`、失敗分のカウント、
削除済み集合、その他）が入る。in-flight だった effect は、完了まで待った
うえで結果を `Completed` と同じルールで詰める。実 AWS API call の冪等性が
保てない以上、in-flight を途中で諦める seam は持たない方が誠実。

呼び出し側は次のように書く。

```rust
let outcome = execute_plan(provider, input, &observer, token).await;
let (result, cancelled) = match outcome {
    ExecutionOutcome::Completed(r) => (r, false),
    ExecutionOutcome::Cancelled(r) => (r, true),
};
finalize_apply(state_file, result, ...).await?;
backend.release_lock(&lock_info).await?;
if cancelled {
    return Err(AppError::Interrupted);
}
```

`?` で握り潰せる単純な `Result<ExecutionResult, _>` ではないので、
caller が cancel を見落とすとコンパイルエラーになる。`was_cancelled: bool`
を生やすだけの代替案は、フラグを参照しない caller が書けてしまう点で同じ
broken state を再現できるため採らない。observer に hook を生やす案も
同様で、observer 実装ごとに「state を回収するのは自分の仕事か」を覚える
必要が出るので採らない。

apply.rs / destroy.rs どちらも同じ seam を通る。**この点が単なる
「per-site で書き換える」ではなく「seam を 1 つに揃える」になっている**
ので、将来 import などの新しい mutating コマンドが追加されても、
`execute_plan` を呼ぶ caller である以上 `Cancelled` を必ず捌くことに
なる。runtime convention に依存しない。

### Cancel のセマンティクス

cancel token が fire したら:

- まだ投入していない effect は捨てる
- in-flight の effect は完了まで `await` する
- 完了した結果は `Completed` と同じ判定基準で `applied_states` に詰める
- すべての in-flight が捌けたら `Cancelled(result)` を返す

「in-flight も即座に諦める」案は、AWS API call を発行した時点で
リソースが AWS 側に物理的に生まれている可能性があり、その生成を
state に記録しないと issue #3498 の症状を再現する。AWS API call は
HTTP リクエストを送ってしまえば carina から止められないという物理的
制約があるので、「呼んだ以上は結果を待って state に書く」を正しい
方針とする。

cancel から見て「shutdown が長引く」問題は CI のキャンセル猶予側で
吸収する話で、carina の seam にこの種の制約を漏らさない。実運用上は
in-flight が完了する数十秒〜数分のうちに次のシグナルが来ない限り、
2 段階目の `process::exit(130)` には届かない。届いた場合は次節の
「2 回目のシグナル」に倣う。

### 統一シグナルハンドラ

SIGINT と SIGTERM の両方で同じ `CancellationToken` を fire する
ハンドラに置き換える。現在の `signal::run_with_ctrl_c` の
`tokio::select!` を使う実装は捨てる。

具体的な形:

- `main.rs` 立ち上がりで `CancellationToken` を生成し、apply/destroy
  などのトップレベルに引き渡す
- `signal_hook` あるいは `tokio::signal::unix` の `SignalKind` で
  SIGINT と SIGTERM を待つ tokio task を 1 本立て、初回シグナルで
  `token.cancel()` を呼ぶ
- 同じ task の中で 2 回目のシグナルを待ち、来たら
  `crate::cursor::restore_cursor()` + `std::process::exit(130)` を実行
- カーソル復元のための `signal_hook` ハンドラはこの中に統合する。
  cursor.rs の独立した signal ハンドラ登録は撤去できる

`run_with_ctrl_c` は `Future` 単体を select でラップする抽象だった
ので、呼び出し側を `cancel_token.clone()` を渡す形に書き換える際に
撤去する。confirm prompt 中の cancel も同じ token を見るように
`read_line_with_interrupt` を `read_line_until_cancelled` 風に
変える。

### apply.rs の流れ

```text
ロック取得
  └─ execute_plan(cancel_token).await
       ├─ Completed(result) → finalize_apply → state 保存 → ロック解放
       └─ Cancelled(result) → finalize_apply → state 保存 → ロック解放 → Interrupted
```

state 保存とロック解放は中断経路でも同じコードパス。
`Cancelled` の場合だけは最後に `AppError::Interrupted` を返して、終了
コードに反映させる。

destroy.rs にも同じ書き換えを入れる。両者は plan を実行する mutating
コマンドという点で同じ shape を持つ。

## 何を直さないか

streaming state save、すなわち 1 effect ごとの state flush は今回の
スコープから外す。「graceful な cancel 後に in-flight 完了を待つ」を
入れた時点で、CI step の猶予内で起きる SIGTERM / SIGINT による損失は
カバーできる。SIGKILL や kernel OOM、ホスト消失のような「猶予が無い」
中断はそもそも graceful な cleanup の対象外で、streaming save でも
完全には救えない。streaming にすると S3 への CAS 書き込みや serial bump
の頻度設計、トランザクション境界の見直しが必要になり、性質の違う仕事
なので別 issue で扱う。

issue #3498 本文に書いてある「force-unlock サブコマンド」は既存
（`carina force-unlock <lock-id>`）。新規実装は不要。今回の修正でロック
残留の頻度自体が落ちる前提なので、エラー文言の `force-unlock` 案内は
そのまま流用する。

drift 検出や live AWS との reconciliation も別議論。今回は「carina が
自分で作ったリソースを state に書き残す」だけを担保する。

## 影響範囲

主に手を入れる場所:

- `carina-core/src/executor/`: `execute_plan` のシグネチャ、戻り値型
  `ExecutionOutcome` の追加、cancel token を見る制御フロー
- `carina-cli/src/signal.rs`: `run_with_ctrl_c` 撤去、cancel token を
  fire する統一ハンドラ
- `carina-cli/src/commands/apply/mod.rs`: 戻り値 match、finalize 経路の
  整理
- `carina-cli/src/commands/destroy.rs`: 同上
- `carina-cli/src/cursor.rs`: cursor restore のシグナルハンドラを統一
  ハンドラに統合（独立登録の撤去）
- `carina-cli/src/main.rs`: cancel token の生成と引き渡し
- `Cargo.toml`: `tokio-util` を `carina-cli` と `carina-core` の依存
  に追加（あるいは自前の軽量 CancellationToken を carina-core 内に置く
  かは実装フェーズで Codex に判断させる）

import / state surgery 系コマンドは `execute_plan` を呼んでいないため
影響無し。ただし将来 mutating コマンドを追加するときは同じ seam を通る
ので、自動的に救済される。

## テスト戦略

中断の seam そのものを compile-time で守れる設計なので、テストは
「seam が起動した時に何が起きるか」を確かめる単純な統合テストになる。

`carina-core` 側で:

- `execute_plan` が cancel 通知を受けて `Cancelled(result)` を返すこと
- `Cancelled.0.applied_states` に cancel 前に完了済みだった effect の
  結果が含まれること
- in-flight だった effect の完了結果も含まれること
- まだ投入していない effect は含まれないこと

`carina-cli` 側で:

- SIGINT で `finalize_apply` 経路が走り、state ファイルが更新されている
  こと、ロックが解放されていること、終了コードが `Interrupted` 由来で
  あること
- SIGTERM でも同じこと
- 2 回目の同じシグナルで `process::exit(130)` 経由で即死すること
  （カーソル復元は通っていること）

state バックエンド側のテストは現状の `acquire_lock` / `release_lock` を
追加変更しないので不要。

issue #3498 を厳密に再現するには直接 mock provider を使った
`carina apply` の統合テストで cancel token を fire させ、その後 state
ファイルが正しく書かれていることを assert する。CI で reliable に
シグナルを送るのは難しいので、`signal` モジュールを経由しない直接の
cancel token fire でテストを書く形になる。

## 失敗モードと残るリスク

- in-flight effect が API レイヤで stuck している場合、cancel 後の
  shutdown が in-flight 完了までかかる。これは AWS 側の挙動依存で、
  carina で短くするには provider trait に cancel を流す別設計が必要。
  scope 外。
- SIGKILL / kernel OOM では graceful shutdown 自体が走らないので、
  state も書かれないし、ロックも残る。これは TTL ベースのロック自動
  期限切れと既存の `force-unlock` で復旧する想定で、コードでは追加
  対応しない。
- `Cancelled(result)` の `finalize_apply` 中に二度目のシグナルが来て
  `process::exit(130)` した場合、state 書き込みの途中で死ぬ可能性が
  ある。state ファイル書き込みは local backend では tempfile + rename
  で atomic、S3 backend では単発 PutObject なので、結果としては
  「途中で死んだら state は古いまま」になる。これは現状と同じ挙動で
  退行ではない。
