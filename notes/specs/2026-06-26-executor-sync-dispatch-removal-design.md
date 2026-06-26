# Executor の同期 await 経路を取り除く

- Issue: carina#3606（cert 作成直後の沈黙ハング）
- 関連: carina#3554（deferred-for-loop の apply 順序）、carina#3599（writeback の deferred-replace 衝突）
- 状態: 設計案

## 何が起きているか

`carina apply` が `+ aws.acm.Certificate cert` の `✓` を出した直後から
何もしないまま固まる。AWS 側では cert は ISSUED まで進み、続く
LB / Route53 / wait のリクエストは一つも飛ばない。SIGINT も効かない。

挙動を計器付きビルドで取った結果、deadlock は executor 内部で完結
していることがはっきりした。決定的シーケンスは以下のとおりで、
`<executor>::execute_effects_sequential`（`carina-core/src/executor/parallel.rs`）の
ループと、`Effect::DeferredReplace` の同期ディスパッチ
（`carina-core/src/executor/deferred_dispatch.rs::dispatch_deferred_replace`）
の組み合わせで起きる。

```
loop#1: cert (idx=2) と alb (idx=3) を push_non_wait
loop#1 AWAIT next_completed         ← FuturesUnordered::next を await

[cert future の poll]
  resolve_resource → ProviderRouter::normalize_desired(&mut [cert])
    aws normalizer  : 0x14f523f00 を lock → 解放
    awscc normalizer: 0x10e9139d0 を lock → 解放
  provider.create(op=create, instance=0x10e9139d0)
    → lock 取得 → AWS API 6.5s → MutexGuard drop で解放

[alb future の poll]               ← cert の AWS 待ち中に並行で poll される
  resolve_resource → ProviderRouter::normalize_desired(&mut [alb])
    aws normalizer  : 0x14f523f00 を lock → 解放
    awscc normalizer: 0x10e9139d0 → LOCK_WAIT
                                  ← cert.create が保持中の Mutex を待つ

cert.create 完了 → finished_idx=2 で next_completed が返る
loop#1 RETURN

loop#2: DeferredReplace (validation_records) が ready → 同期ディスパッチ
  dispatch_deferred_replace.await
    delete#0 (aws.route53.RecordSet) を実行
      LockedStore::acquire(op=delete, instance=0x10e9139d0)
                                  ← 永久ハング
```

`tokio::sync::Mutex` の wait list には alb の awscc-normalize が先に
入っていて、cert が解放すると wake されるが、cert の完了で
`FuturesUnordered::next` が `Ready` を返してしまい、loop は同じ async
タスクの中で `dispatch_deferred_replace.await` の中に入る。この間
`in_flight` の中の alb future は誰にも poll されない。wake は届いた
のに poll されないので `Mutex::lock` は取得状態に遷移できず、続いて
入ってきた delete はその後ろに並ばされて永久に待つ。

つまり症状の根は scheduler ループの設計にあり、HTTP の遅さでも、
WASI HTTP のタイムアウト不足でもない。`with_operation_timeout` の
20 分タイマーは provider 呼び出しの中の future にしか効かないので、
provider 呼び出しに到達できていない alb と delete には届かず、
20 分後にも自動回復しない。

## 不変条件をどう壊しているか

executor の暗黙の不変条件はおおむね次のとおりだった。

1. apply 中の I/O やロック取得を伴う処理は、すべて `in_flight`
   （`FuturesUnordered`）に乗ったうえで `next_completed` の poll を
   通じて進める。
2. scheduler のループ本体は、`in_flight` の中の future が同じ task
   コンテキストで poll される機会を奪わない。

ところが `dispatch_deferred_replace` は次の理由でこの (2) を破る。

- `Effect::DeferredReplace` を見つけたら、`for` ループを break して
  `completed_synchronous_dispatch = true; continue;` で戻る前に、
  そのままディスパッチャを `.await` する。
- ディスパッチャの中で `stream::iter(deletes)
  .buffer_unordered(concurrency).collect().await` を await して
  delete 半分の HTTP を呼び、終わったら `dispatch_deferred_create`
  で create 半分を materialize する。
- この `.await` の間、scheduler ループ本体は `next_completed` を
  呼ばないので、`in_flight` に積まれている他の future（今回の alb）
  は poll されない。

`Mutex::lock` の wake 通知が届いているのに poll されないと、
acquire は完了しない（取得は次回 poll で起きる）。同じ Mutex に
後から並んだ別の lock 取得（DR の delete）は、当然その後ろになり、
誰もキューを進められず deadlock になる。

複数 provider のリソースが並列に in_flight にある場合だけが問題
ではない。`ProviderRouter::normalize_desired` が常に登録済み
normalizer 全部に対して順番に `.await` するため、同じ provider 内
でも別 resource どうしが互いの WASM ストアを同じ順序で取りに行く
状況は容易に起きる。今回顕在化したのは aws と awscc を跨ぐ形だが、
たとえば `aws.acm.Certificate` と `aws.route53.RecordSet` を同じ
in_flight 集合に並べた瞬間に、同じ aws の Store Mutex を取り合って
同じハングが再現する。**症状を「複数 provider が混ざったとき」に
限定する読み方は誤り**で、scheduler が同期 await している間に
in_flight が止まることそのものが根本原因。

## 修正方針 — scheduler の同期 await 経路を全廃する

executor の不変条件を、コードで書き直したうえで型レベルで保てるか
たちにし、同期 await の穴を消す。

具体的には次の二つに分けて考える。

### A. I/O や lock を伴う meta-effect は `in_flight` に乗せる

`Effect::DeferredReplace` を `in_flight` の中で並行に poll してもらう
形に変える。手順は executor 入口で次のとおりに展開する。

1. plan 段階の `Effect::DeferredReplace` は今までどおりプランや
   表示の側に届ける（display や plan_tree は触らない）。
2. `execute_effects_sequential` が effects を受け取った直後に、
   DR を見つけたら **scheduler の内部表現上だけで**「delete 半分の
   `Effect::Delete` ×N」と「create 半分を materialize するための
   gate」に分解し、`effects` / `actionable_indices` / `deps_of` の
   配線をそれに合わせて書き換える。
3. 分解後の delete は通常の `Effect::Delete` と完全に同じ経路で
   `in_flight` に push し、`buffer_unordered` を経由せずに scheduler
   が直接並列ディスパッチする。
4. create 半分の materialize gate は、upstream binding と分解済みの
   全 delete idx に依存する scheduler-meta effect として `in_flight`
   に乗せる必要はない。**materialize は I/O を伴わない純粋な処理**
   なので、ready になった瞬間に同期で実行し、child Create を effects
   に push し、`completed_indices` に gate idx を入れて即座にループ
   先頭に戻ればよい。ここで gate の同期実行が許されるのは、
   materialize の中で `Mutex::lock` や HTTP が一切走らないという
   局所的事実が根拠で、これは関数の型として表現する（後述）。

これによって「I/O を伴うのに scheduler ループ本体で `.await` される
処理」は消える。delete 半分は通常の Delete 経路と同じく `in_flight`
に積まれて scheduler が `next_completed` で駆動する。

### B. 「同期 dispatch は純粋な処理に限る」を型で表現する

A の (4) で「materialize gate は同期に呼んでいい」と言ったが、
これを将来の変更者が壊すのを防ぐ。`materialize_deferred_create`
の型を `fn(...) -> Result<Vec<Effect>, _>`（**`async fn` ではなく
同期関数**）に固定し、関数本体から provider/normalizer/HTTP に
触れる手段を一切持たせない。具体的には次の typestate に切り分ける。

```rust
// I/O を含むディスパッチ。必ず in_flight 経由で動かす。
pub(super) enum InFlightStep<'fut> {
    Future(Pin<Box<dyn Future<Output = (usize, SingleEffectResult)> + 'fut>>),
}

// 純粋な scheduler 内処理。同期で呼んでよい。新しい effects を
// 返すだけで、Future を返さない。
pub(super) struct PureMetaStep {
    pub(super) materialize: fn(/* upstream, template, bindings */)
        -> Result<Vec<Effect>, DeferredCreateFailure>,
}
```

DR の create 半分の gate と DC は `PureMetaStep` を返す。Create /
Update / Delete / Replace / Wait は `InFlightStep` を返す。
`execute_effects_sequential` のディスパッチ箇所は **どちらが返って
くるかで完全に分岐**し、`.await` を呼ぶのは `InFlightStep` だけ。
これによって「scheduler のループ中に I/O 系 `.await` が混じる」状態
が**型として書けない**ようになる。

新しい effect 種別が追加されたときも、この二択のどちらかに分類
することが強制される（match の網羅性チェックで未分類が出る）。
将来 DR とは別の deferred 系 meta-effect を足すときに、今回と同じ
class のバグを再導入できない。

## 修正の射程

- **A は in-PR で全部やる**。delete 半分を `in_flight` に乗せる
  リファクタと、materialize gate の同期実行への置き換えを一緒に
  当てる。`Effect::DeferredReplace` を残したまま executor 内部で
  分解する形を取れば、plan / display / writeback 側は今回の PR で
  触らずに済む。CLAUDE.md の「typed reshape は in-PR で、scope で
  逃げない」原則に沿って、scheduler の同期 await 経路は同じ PR で
  消し切る。
- **B も in-PR で**。`InFlightStep` / `PureMetaStep` の typestate
  分離を入れずに A だけ済ますと、将来別の meta-effect を足したとき
  に同じバグの再発を許す（=「新しい呼び手が出てきたら同じ過ちを
  繰り返せる」状態が残る）。「broken state unrepresentable」を満た
  すには両方必要なので分割しない。
- `dispatch_deferred_create` の同期パス（DC, DeferredCreate）は
  もともと純粋処理なので、A の変更で gate と同じ `PureMetaStep`
  に揃える。ここを揃えておくと、DR の create 半分の gate と DC が
  ひとつの型で表現されて scheduler 側の場合分けが減る。

## 再現テスト

設計のとおりに直しても、計器でとった現象が消えていることをユニット
テスト相当で固定する。`carina-core/src/executor` 配下に、

- aws / awscc の二つの mock provider を立てる
- それぞれの normalizer は `tokio::sync::Mutex<()>` を一つずつ持ち、
  `normalize_desired` の中で短い `lock().await` を取る
- aws.acm.Certificate に相当する mock resource と awscc 側の
  LoadBalancer 相当を同じ in_flight に並べる
- そのうえで DR（delete 1 件）を、cert に依存する形で plan に
  載せる

再現テストでは、修正前のコードでは `tokio::time::timeout(5s, ...)`
で apply が `Err` になり、修正後では成功して child Create まで
進むことを assert する。タイミング依存を避けるため、mock の
ロック取得順序を制御できる仕掛け（barrier）を入れる。

## やらないこと

- `ProviderRouter::normalize_desired` を「resource の所属 provider
  だけ呼ぶ」形に変える案（先のメモの C 案）はやらない。今回のケース
  は止められるが、scheduler の同期 await が残る限り同 provider 内
  の競合で同じ deadlock が再発するので、症状を移すだけになる。
- WASI HTTP のタイムアウトを短くする系の修正はやらない。HTTP は
  そもそも飛んでいないので筋違い。
- `with_operation_timeout` の 20 分を短くする話もやらない。
  同上で、ここに来る前にハングしている。

## 影響範囲

- 触る範囲は `carina-core/src/executor/parallel.rs` と
  `carina-core/src/executor/deferred_dispatch.rs`、それと再現テストの
  追加（`carina-core/src/executor/` 配下）。
- `Effect` enum の variant、`Plan`、display、`carina-cli`、provider
  trait は触らない。plan 出力や `carina.state.json` への副作用は
  ない。

## 実装 PR を切る前に

設計 PR をまず開いて merge してから、実装 PR を一本立てる
（CLAUDE.md「design PR は実装 PR より前に merge」）。
実装は Codex に投げる（CLAUDE.md「実装作業は Codex に委ねる」）。
本ドキュメントが merge されるまで実装ブランチには手をつけない。
