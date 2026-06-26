# phased / parallel スケジューラ統一設計

- 日付: 2026-06-26
- 対象 Issue: carina#3611, #3612, #3613, #3614, #3615
- 前提 PR: #3607 / #3608 (`executor-sync-dispatch-removal-design`),
  #3610 (validate wait indexing)

## 背景

#3608 で executor を「sync `.await` を取らずに `Effect::DeferredReplace`
を分解し、expansion 時に張った delete-dep edge と
`failed_indices: HashSet<usize>` で順序と失敗伝播を制御する」形に
書き換えた。ただし、書き換えが届いたのは `parallel.rs` (interdependent
Replace が無いパス) だけで、`phased.rs` (interdependent Replace を
含む計画が通るパス) は #3608 の前と同じ構造、すなわち
`failed_bindings: HashSet<String>` を使った name ベースの failure
集合と、`apply_deferred_replace_delete_deps` を各サイトで手呼び出す
convention で残った。

結果として phased 経路には以下が同居している。

- DeferredReplace の絶対 delete 失敗を gate が見落とし、子 Create を
  materialize してしまう回帰 (#3611)
- 同じ relax → overlay の手順を 7 サイトで毎回手書きする convention
  seam (#3612)
- DC / DR dispatch arm を 4 サイトでコピペした重複 (#3613)
- `failure_binding_name` を 4 サイトでインライン展開した重複 (#3614)
- string と index の混在 bookkeeping (#3615)

CLAUDE.md の Part 1 にある「sibling code path の同種バグは同じ PR で
潰す」「broken state を type で表現不能にする」原則に照らすと、5 件は
別々の bug ではなく一つの sibling-drift である。本書ではその 1 件を
「phased 経路を parallel 経路に追従させて、両者が **同一の scheduler
primitive を共有する**形にする」設計として整理する。

## 何が壊れているか

### parallel 経路 (現状の正解)

dispatch ループは次の不変条件で動く。

- 失敗集合は `failed_indices: HashSet<usize>`。effect index がそのまま
  failure の identity となる。匿名 Replace / Move / Remove も index で
  乗るので脱落しない。
- 失敗依存の判定は `find_failed_dependency_index(idx, &deps_of,
  &failed_indices, &effects)`。deps graph は
  `build_scheduler_deps(...)` が「analysis → relax → DR delete-dep
  overlay」を一括で構築する。
- DC / DR の dispatch は `PureMetaStep::from_effect(effect)` が
  「これは pure-meta か」を typestate で判定し、Some を返したら共有
  ヘルパで `dispatch_deferred_create` を呼ぶ。caller は変種ごとの
  分岐を持たない。
- failure 表示や Wait 終端判定に必要な「binding 名」は
  `failure_binding_name(&Effect)` で取り出す。DC/DR の場合は
  `template.binding_name` を返し、その他は `Effect::binding_name()`
  に委譲する。Wait の終端判定 (`count_effectively_undispatched`) は
  `failed_indices` から `failed_binding_names` を毎回派生させて渡す
  (これだけは派生のままで、#3615 の対象。後述)。

### phased 経路 (現状の壊れ方)

`execute_effects_phased` の Phase-1 wave (`~543-594`) と wave-2
(`~1641-1690`) がそれぞれ独立に dispatch ループを持っていて、
parallel との差分が次の通り蓄積している。

1. `failed_bindings: HashSet<String>` だけを使う。expansion 時に
   入る匿名 delete 半身 (`validation_records[0]` のような) は
   `failure_binding_name` を呼ばれず string 集合に入らないので、
   後段の `find_failed_dependency` (name キー) が dep miss と判定し、
   DeferredReplace gate がそのまま通る → child Create を materialize
   する。これが #3611 の本体。
2. 「analysis → overlay」を `apply_deferred_replace_delete_deps(&mut
   deps_of, &deferred_replace_delete_deps)` の手書きで 7 サイト繰り
   返している。一つ新規 site が増えたら必ず忘れる convention seam
   (#3612)。
3. DC / DR dispatch arm が 4 サイトでバイト一致のコピペ (#3613)。
4. DC / DR の failure を記録するときに `failed_bindings.insert(
   template.binding_name.clone())` を 4 サイトでインライン (#3614)。
5. `count_effectively_undispatched` と `cancel_waits_if_terminal` が
   name 集合のみを参照する。これは parallel との対称性 (#3615 の派生
   ステップ) を取るために phased でも index ベースに合わせる必要が
   ある。

つまり 5 件は「parallel に存在する scheduler primitive が phased から
見えていない」という一根の症状である。

## 設計

### 一根の解

`parallel.rs` の冒頭に置かれている次の private 要素を、両モジュールが
import できる位置 (推奨: 既存の `super::deferred_dispatch` を
`super::scheduler` にリネーム、もしくは新規 `super::scheduler.rs` を
新設) に引き上げる。

- `PureMetaStep<'a>` と `PureMetaStep::from_effect`
- `failure_binding_name(&Effect) -> Option<String>`
- `build_scheduler_deps(...)` (内部で `apply_deferred_replace_delete_deps`
  を呼ぶ)
- `find_failed_dependency_index(idx, &deps_of, &failed_indices, &effects)`
- `failed_binding_names(&effects, &failed_indices)`

`parallel.rs` 側ではこれらを `pub(super)` に変えて再露出し、
`phased.rs` 側は同じ import を経由する。`apply_deferred_replace_delete_deps`
は既に `pub(super)` で `phased.rs` から呼ばれているが、共有モジュール
からの呼び出しを正規化して、`phased.rs` 内のすべての
`build_dependency_analysis` 呼び出しサイトは `build_scheduler_deps`
にまとめる。

### `Effect::binding_name()` は触らない

#3614 の選択肢 (a) は `Effect::binding_name()` の DC / DR arm を
`Some(template.binding_name.clone())` に変える案だったが、これは取らない。
理由は二つ。

1. `effect.rs` の doc-comment が「DC / DR は expansion 前なので
   resource identity を持たない。`binding_name()` は identity 専用」と
   明示している。失敗時の表示名は identity ではなく "diagnostic 用の
   人間可読 hint" であり、別 API が筋。
2. `binding_name()` の他 caller (`carina-cli` の plan display, state
   summary など) はすべて identity 用途で呼んでいる。ここを変えると
   plan / state 表示にも DC / DR template binding が紛れ込み、別の
   表示バグを連鎖して作る。

選択肢 (b) ──「scheduler-internal helper として共有」── を採る。
これは convention 共有ではなく **API shape** の共有である:
`failure_binding_name` は scheduler 専用の private fn として共有
モジュールに置き、phased / parallel 双方がそこから import する。
phased 側で 4 ヶ所に書かれていた `failed_indices.insert(idx)` +
`failed_bindings.insert(template.binding_name.clone())` 相当の処理は、
共有ヘルパ `record_failed_effect(idx, effect, &mut failed_indices)`
にまとめ、name 集合は持たなくなる (#3615 で派生に切り替えるため)。

### phased 経路の failure 集合を index ベースに統一

#3611 と #3615 はどちらも「phased の string-only bookkeeping」を
原因にしている。両方を同時に直すには、

- `failed_bindings: HashSet<String>` を削除。代わりに
  `failed_indices: HashSet<usize>` を持つ。
- 失敗依存の判定は `find_failed_dependency_index` (共有版) を使う。
  既存の `find_failed_dependency(effect, &failed_bindings)` の
  呼び出しは全廃。
- `count_effectively_undispatched(...)` と
  `cancel_waits_if_terminal(...)` の caller では、name 集合を必要と
  する箇所だけ `failed_binding_names(&effects, &failed_indices)` で
  毎回派生させて渡す (parallel と同じパターン)。Wait の API 内側を
  index 直接受け取りに変えるのは今回の PR では行わない (Wait 側 API
  変更は #3615 が parallel に対してもまだ残している宿題で、別の
  追加 PR で両モジュール同時に行うほうが radius が小さい)。

ただし parallel 側にも `failed_binding_names` を毎回呼んでいる派生
ステップは残っているので、これは将来「Wait の API を index 直接に
変える」追加 PR (= #3615 の本来の解決) で両モジュール同時に消す。
今回はその準備として、phased を parallel と同じ「派生はここ、保持は
index」という対称構造に揃える。これにより #3615 の本来の解決を
両モジュール同時に進められるようになる。

### `apply_deferred_replace_delete_deps` の手呼び出し撲滅

phased.rs の以下 7 サイト (build_dependency_analysis 呼び出し直後) を
すべて `build_scheduler_deps(...)` 1 呼び出しに置き換える。

- Phase-1 wave 入口の deps 構築
- Phase-1 wave での DC 後の deps 再構築
- Phase-1 wave での DR 後の deps 再構築
- wave-2 入口の deps 構築
- wave-2 での DC 後の deps 再構築
- wave-2 での DR 後の deps 再構築
- (もう 1 サイト、build_dependency_analysis を直接呼んでいる箇所)

`build_dependency_analysis` 自体は touch しない。これは下位の分析
プリミティブとして残し、scheduler 経路は必ず `build_scheduler_deps`
を経由する原則を type ではなく module 境界で表す。理想的には
`build_dependency_analysis` を `pub(super)` から
`super::scheduler` モジュール内 private に閉じて
「scheduler 以外から呼べない」状態にできるとなお良いが、現状
`build_dependency_analysis` は他にも caller があるかもしれないため
それは別作業 (この PR の範囲内で確認して、可能なら閉じる)。

### DC / DR dispatch arm の重複削除

phased の 4 サイトの DC / DR arm は次の同じ shape を踏む:

1. `PureMetaStep::from_effect(&effect)` で pure-meta 判定
2. Some なら `dispatch_deferred_create(...)` を呼ぶ
3. `Materialized(children)` なら effects に append + deps を
   `build_scheduler_deps` で再構築 + observer event を発火
4. `MaterializeFailed` なら `failed_indices.insert(idx)` (共有
   `record_failed_effect` 経由)

この 4 サイトを `try_dispatch_pure_meta(...)` (仮称) という共有 fn に
くくり、4 ヶ所はその呼び出し 1 行に圧縮する。fn の戻り値は
`enum PureMetaOutcome { NotPureMeta, Materialized, Failed }` で
caller が dispatch flow を 1 つの match で続けられる shape にする。

### 1 PR にまとめる根拠 (CLAUDE.md 適合)

5 件のうち #3611 は本番事故になり得る回帰だが、その回帰の root cause
が「phased が parallel に追従していない」一点であり、#3612–#3615 は
すべて同じ追従の異なる側面である。CLAUDE.md「sibling 経路の同種バグ
は同じ PR で潰す」「root cause を直すのが PR の topic」を厳密に取る
と分割は不可。#3611 を急ぐために #3612–#3615 を follow-up に回すと、
phased を再び触る次の PR が「parallel 追従を忘れる」リスクを残す。

review の重さは hold する側のコスト、本番事故になり得る回帰は
hold しない側のコストで、今回は後者を取る判断。

### Out of scope

- Wait API の index 直接受け取り化 (#3615 の本来の解決): phased
  側も parallel 側も派生ステップで耐えているので、本 PR では両者を
  対称にして打ち消し合うステップを次の PR で揃って消せる状態にする
  に留める。
- `build_dependency_analysis` を scheduler モジュール private に
  閉じるリファクタ: caller 調査を行い、scheduler 以外に caller が
  いなければ本 PR に含めて良い。caller がいれば別 issue 化。
- `Effect::binding_name()` の DC / DR 戻り値変更: 別 API caller に
  影響するため対象外。

## TDD: 先に書く回帰テスト

`carina-core/src/executor/tests.rs` に以下のテストを足す。

### `phased_deferred_replace_gate_skips_on_absorbed_delete_failure`

shape:

- plan に interdependent Replace を含めて phased 経路を強制
  (`has_interdependent_replaces()` が true になる fixture)。
- 同時に `Effect::DeferredReplace` を含めて、その absorbed delete
  半身の 1 つを provider が Failure として返すように mock を組む。
- 期待値:
  - apply 完了時、DR の child Create が **作成されていない**
    (provider 側の Create count = 0)
  - failure summary に絶対 delete の失敗が報告されている
  - 既存リソースの存在は維持されている (= duplicate / orphan が
    発生していない)

このテストは現行 phased.rs では fail する (gate が抜け、child
Create が発火する) ことを赤として確認してから、parallel 追従の
fix を当てて緑にする。

### 既存テストの sanity

parallel 側に既に入っている類似テスト
(`executor::tests` 内) を grep して、phased 版が無いものは合わせて
phased 用にも書く。最低限:

- DC failure 時に child Create が出ないこと (phased 版)
- 匿名 Replace 失敗が downstream Wait に dependency-failed として
  伝わること (phased 版、#3611 と #3615 の合流確認)

## 着手順序

1. 本設計書 (本ファイル) を別 PR として先に出す (Design PR before
   implementation PR ルール)。design PR の merge を待ってから実装に
   進む。
2. 共有モジュールの抽出 (parallel.rs → 共有モジュール) を Codex に
   依頼。parallel 経路の挙動が変わらないことを確認する nextest 緑が
   ゲート。
3. phased.rs の `failed_bindings` 廃止と `failed_indices` 化を Codex
   に依頼。同時に 7 サイトの `build_scheduler_deps` 化、4 サイトの
   DC / DR dispatch 統合、4 サイトの `failure_binding_name` 共有化を
   一括で当てる (1 commit でなくて良いが 1 PR)。
4. 回帰テスト (上記 2 本) を Codex に書かせて、赤を確認してから fix
   を入れる順序を守る。
5. verify → /code-review medium → /self-review 5 周 → PR → CI → merge。

## 関連メモ

- 設計ドキュメント: `notes/specs/2026-06-26-executor-sync-dispatch-removal-design.md`
  (#3608 で merge 済み)
- handoff: `memory/handoff_2026-06-26_post_3606_executor_cleanup.md`
- 共有モジュール候補: 既存 `carina-core/src/executor/deferred_dispatch.rs`
  にまとめるか、別途 `carina-core/src/executor/scheduler.rs` を立てる
  かは Codex 着手時に決める。後者なら「scheduler primitive 全部の
  集合」として読みやすく、前者なら新規ファイル無し。後者を推奨。
