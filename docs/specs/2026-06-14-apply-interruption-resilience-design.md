# Apply 中断時のリソース永続化とロック解放（設計）

対象 issue: [carina-rs/carina#3498](https://github.com/carina-rs/carina/issues/3498)、
[carina-rs/carina#3542](https://github.com/carina-rs/carina/issues/3542)

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

### #3542 で判明した追加の中断経路

#3498 の初期修正後は SIGINT と SIGTERM の両方を受け取れていたが、signal
listener は command future から独立した task のままだった。初回シグナルは
cancel token を fire する一方、2 回目は `std::process::exit(130)` を直接呼ぶ。
この形では、command future が state 保存とロック解放をまだ実行中でも listener
だけでプロセスを終了できる。

GitHub Actions のキャンセルは人間による連打ではなく、固定の
SIGINT → 7.5 秒 → SIGTERM → 2.5 秒 → SIGKILL という sequence である。
したがって 2 回目の SIGTERM は例外ではなく毎回到達し、旧 listener は残りの
約 2.5 秒を cleanup に使わず即座に捨てていた。#3542 の修正ではこの 2 回目を
「即時終了」ではなく「cleanup 優先」への phase 遷移として扱う。

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
    shutdown: ShutdownToken,
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

初回シグナルで graceful shutdown が fire したら:

- まだ投入していない effect は捨てる
- in-flight の effect は完了まで `await` する
- 完了した結果は `Completed` と同じ判定基準で `applied_states` に詰める
- すべての in-flight が捌けたら `Cancelled(result)` を返す

graceful phase で「in-flight も即座に諦める」案は、AWS API call を発行した時点で
リソースが AWS 側に物理的に生まれている可能性があり、その生成を
state に記録しないと issue #3498 の症状を再現する。AWS API call は
HTTP リクエストを送ってしまえば carina から止められないという物理的
制約があるので、「呼んだ以上は結果を待って state に書く」を正しい
方針とする。

2 回目のシグナルでは cleanup-priority phase を fire する。この phase では
未完了の in-flight future を drop し、それまでに `ExecutionResult` へ回収済みの
結果だけを返す。command path はその結果で state を保存してロックを解放する。
これは通常の graceful cancel より結果回収を狭める trade-off だが、GitHub
Actions が SIGKILL するまでの残り 2.5 秒で、回収済み state とロックを守るための
明示的な escalation である。このとき放棄した Create は、provider future の結果を
回収する前に AWS API が成功していれば、AWS 側では作成済みなのに state には存在しない
状態になり得る。次回 apply が同じリソースを重複作成し得る、issue #3498 と同じ危険を
cleanup-priority は意図的に受け入れる。したがって graceful phase ではこの放棄を行わず、
2 回目のシグナル後に state とロックを守る最終手段としてだけ使う。

### 統一シグナルハンドラ

SIGINT と SIGTERM の両方を command-scoped supervisor で扱う。
`CancellationToken` だけを持つ独立 task や、command future と競争してそれを
drop する `tokio::select!` は持たない。

具体的な形:

- `main.rs` の command dispatch 全体を `run_with_shutdown` に渡す
- `carina-core` の supervisor が private な書き込み capability と read-only な
  `ShutdownToken` を生成し、command future には token だけを渡す
- 初回シグナルで graceful shutdown、2 回目で cleanup priority を request する
- 2 回目から command future の完了と 2 秒の hard-coded deadline を競争させ、
  どちらかに到達した後で `std::process::exit(130)` を呼ぶ。2 秒は GitHub
  Actions の固定された SIGTERM → 2.5 秒 → SIGKILL の窓に収めるためで、CLI
  flag や環境変数にはしない
- 2 秒のうち最初の 1 秒を state 保存に割り当て、後半 1 秒をロック解放用に
  予約する。state 保存が遅くてもロック解放を開始する機会を失わせない
- S3 の cancellation cleanup だけは各 API call を 300ms・1 attempt に制限する。
  normal apply の SDK timeout / retry は変えない。state PUT より先にロックを解放したり
  両者を並行実行したりすると、別 process がロック取得後に旧 process の PUT が着地する
  ため禁止する。代わりに conditional renewal の所有権確認を state PUT に再利用して、
  state 側を lock GET + renewal PUT + state PUT の 3 call、release を GET + DELETE の
  2 call に収め、必ず state → release の順序を保つ
- 3 回目のシグナルだけは emergency escape hatch として即時終了する
- カーソル復元は process exit 実装に統合する

signal-driven な process exit capability と command future を supervisor が
同時に所有する。外部に公開する API は「command future を supervisor に渡す」
形だけなので、cleanup を所有する future と無関係な detached listener を作る
状態は型として表現できない。apply/destroy 固有の opt-in ではなく dispatch 全体の
境界なので、将来 mutating command が増えても自動的に同じ supervisor 配下に入る。

`run_with_ctrl_c` は `Future` 単体を select でラップして drop できる抽象だった
ので撤去する。confirm prompt 中の cancel も同じ `ShutdownToken` を見るように
`read_line_until_cancelled` へ統一する。

### apply.rs の流れ

```text
ロック取得
  └─ execute_plan(shutdown_token).await
       ├─ Completed(result) → finalize_apply → state 保存 → ロック解放
       └─ Cancelled(result)
            ├─ graceful → in-flight 完了を回収
            └─ cleanup priority → in-flight を放棄
               → finalize_apply → state 保存 → ロック解放 → Interrupted
```

state 保存とロック解放は中断経路でも同じコードパス。
`Cancelled` の場合だけは最後に `AppError::Interrupted` を返して、終了
コードに反映させる。

destroy.rs にも同じ書き換えを入れる。両者は plan を実行する mutating
コマンドという点で同じ shape を持つ。

## 何を直さないか

streaming state save、すなわち 1 effect ごとの state flush は今回の
スコープから外す。「graceful な cancel 後に in-flight 完了を待ち、2 回目では
cleanup を優先する」ところまでを対象にする。SIGKILL や kernel OOM、
ホスト消失のような「猶予が無い」
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
- `carina-cli/src/signal.rs`: Unix signal と process exit の adapter
- `carina-cli/src/commands/apply/mod.rs`: 戻り値 match、finalize 経路の
  整理
- `carina-cli/src/commands/destroy.rs`: 同上
- `carina-cli/src/cursor.rs`: cursor restore のシグナルハンドラを統一
  ハンドラに統合（独立登録の撤去）
- `carina-cli/src/main.rs`: command dispatch 全体を supervisor に引き渡す
- `carina-core/src/shutdown.rs`: command future と signal-driven exit を同時に所有する
  supervisor、graceful / cleanup-priority の phase を持つ `ShutdownToken`、2 秒の
  cleanup deadline。phase を進める capability は supervisor 内部だけが持つ
- `Cargo.toml`: `tokio-util` を `carina-cli` と `carina-core` の依存
  に追加（あるいは自前の軽量 CancellationToken を carina-core 内に置く
  かは実装フェーズで Codex に判断させる）

import / state surgery 系コマンドも top-level dispatch として同じ supervisor
配下に入る。executor の phase-aware な中断が必要な処理には `ShutdownToken` を
渡すが、signal listener と cleanup future の lifetime は command 種別によらず
常に supervisor が結び付ける。

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
- 2 回目のシグナルでは cleanup が完了してから exit 130 になること
- cleanup が 2 秒を超えたら deadline 後に exit 130 になること
- state 保存が 1 秒を超えても、残り時間でロック解放を開始すること
- 3 回目のシグナルでは即時に exit 130 になること
- apply と destroy の両方で、cleanup-priority phase に回収済み state の保存と
  ロック解放まで到達すること

state バックエンド側では cancellation 専用 S3 operation が 1 attempt / 300ms に
固定され、3 回の state call と 2 回の release call が 2 秒未満に収まることを検証する。
CLI 側では call ごとの遅延を注入できる fake backend で、slow-but-bounded な全 5 call が
deadline 内に完了することと、state flush が遅い場合も release が実行されることを検証する。

issue #3498 の end-to-end 回帰テストは built binary を subprocess として起動する。
mock provider が後続 Create の開始を readiness file で通知してから実 SIGINT を送り、
stderr の Interrupt 受信行を handshake にして実 SIGTERM を送る。exit 130、完了済み
resource の state 保存、放棄 resource の非保存、local lock file の削除を同時に検証し、
Unix signal 登録と top-level `run_with_shutdown` wiring まで unit-test seam と分けて覆う。

## 失敗モードと残るリスク

- in-flight effect が API レイヤで stuck している場合、cancel 後の
  shutdown が in-flight 完了までかかる。これは AWS 側の挙動依存で、
  carina で短くするには provider trait に cancel を流す別設計が必要。
  scope 外。
- SIGKILL / kernel OOM では graceful shutdown 自体が走らないので、
  state も書かれないし、ロックも残る。これは TTL ベースのロック自動
  期限切れと既存の `force-unlock` で復旧する想定で、コードでは追加
  対応しない。
- cleanup-priority の 2 秒 deadline 内に state 保存とロック解放が終わらない
  場合は exit 130 になる。state 保存は 1 秒で打ち切って後半をロック解放に
  予約する。state ファイル書き込みは local backend では
  tempfile + rename で atomic、S3 backend では単発 PutObject なので、中途半端な
  state は公開しない。ただし deadline 到達時点で未完了の stage は失われ得る。
