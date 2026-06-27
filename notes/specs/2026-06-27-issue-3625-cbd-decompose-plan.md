# Issue #3625 CBD Replace Decomposition Plan

Issue: https://github.com/carina-rs/carina/issues/3625

## 目的

`carina apply` の create-before-destroy replace で、古いリソースを参照する consumer update が `create` と `delete` の間に scheduler 上で見えない問題を直す。採用方針は A-2、つまり通常の `Effect::Replace` を廃止し、plan 上では `Effect::Create` / `Effect::Update` / `Effect::Delete` に分解すること。

期待する実行順は次の形にする。

```text
Create(new replaced resource)
  -> Update(consumers that reference new value)
  -> Delete(old replaced resource)
```

`Effect::DeferredReplace` は今回の対象外とする。構造が異なり、apply 時に create 側が materialize されるため、通常 replace の分解とは別 issue で扱う。

## 現状の根本原因

現在の `Effect::Replace` は、CBD の中で `create -> cascading update -> delete -> optional rename` を executor 内部で逐次実行する。scheduler から見ると Replace は 1 個の effect であり、`cascading_updates` は graph node ではない。

一方、consumer が既に独立した `Effect::Update` として plan に存在する場合、`cascade_dependent_updates` は `planned_ids` に含まれる dependent を `CascadingUpdate` 追加対象から外す。結果として、consumer update は Replace 内部にも外部にも正しい順序制約を持たず、old delete と並列実行され得る。

A-2 では hidden operation をなくす。consumer update、temporary rename update、old delete をすべて plan の effect として表に出し、scheduler の dependency edge で順序を保証する。

## 影響範囲

`carina-core/src/effect.rs`
: `Effect::Replace` と `CascadingUpdate` を最終的に削除する。`Effect::Update.from` は `Box<State>` から `UpdateBase` に置き換える。`Effect::Delete` に `blocked_by_updates: HashSet<String>` を追加し、serde は `#[serde(default)]` にする。`ScheduleEdge::BlockedByIfDelete` は使用箇所確認の結果 Replace 専用なので削除する。

`carina-core/src/differ/plan.rs`
: create-only diff から直接 `Effect::Replace` を作らず、まず `PendingReplace` として保持する。dependent scan 後に CBD/DBD、temporary name、cascade reason を確定し、`decompose_replace_into_effects` で Create/Update/Delete と display metadata を生成する。

`carina-core/src/plan.rs`
: `Plan` に serde 対象の `replace_display: Vec<ReplaceDisplayMetadata>` を追加する。`Plan` は `PlanFile.plan` として saved plan に保存されるため、表示 metadata も `Serialize` / `Deserialize` する。`set_cascading_updates`、`merge_cascade_create_only`、`promote_to_create_before_destroy` は Stage 2 で PendingReplace 側へ移す。

`carina-core/src/effect/deps.rs`
: `BlockedByIfDelete` の解決処理を削除する。`Delete.apply_edges()` は既存 `dependencies` と新規 `blocked_by_updates` の両方から `ScheduleEdge::BlockedBy` を出す。`destroy_edges()` は既存 destroy semantics を守るため `dependencies` のみを使い、`blocked_by_updates` は apply 専用とする。

`carina-core/src/executor/parallel.rs`
: Stage 1 で Replace executor が `cascading_updates` を実行しない前提に合わせる。最終的には `execute_replace_parallel` / `ReplaceContext` / `SingleEffectResult::Replace` を削除し、Create/Update/Delete は basic executor 経由に寄せる。

`carina-core/src/executor/phased.rs`
: Replace 専用 phase、interdependent replace sort、post-replace wait phase を削除または分解済み effect の dependency graph に置き換える。DeferredReplace と Wait の特殊処理は残す。

`carina-core/src/executor/replace.rs`
: Stage 1 では `cascading_updates` 実行を無効化する。Stage 2 以降、rename と cascade update が独立 Update になるため、Replace executor 全体を削除する。`compute_full_diff_patch` と `single_attribute_patch` は必要なら Update executor 側へ移す。

`carina-core/src/executor/scheduler.rs`
: scheduler の基本構造は維持する。分解済み effects が入る前提で failed dependency 表示と dependency map の扱いを確認する。

`carina-core/src/executor/tests.rs`
: Replace fixture を Create/Update/Delete fixture に置き換える。CBD ordering、rename、consumer update failure、old delete failure、partial diagnostic、writeback を分解後の effect 単位で検証する。

`carina-core/src/differ/cascade_tests.rs`
: `CascadingUpdate` の有無を見るテストを、独立 `Effect::Update` の追加または既存 Update の再利用、old Delete の `blocked_by_updates` 付与を見るテストへ置き換える。create-only consumer は replace display metadata 付き Create/Delete pair になることを確認する。

`carina-core/src/differ/diff_tests.rs`
: create-only change が `Effect::Replace` になる期待を、PendingReplace 経由で Create/Delete pair と display metadata が生成される期待へ変える。

`carina-core/src/differ/plan_tests.rs`
: temporary name 生成テストを、Create resource が temporary name を持つこと、`can_rename=true` なら `UpdateBase::CreatedBy` の rename Update が追加されることの確認へ変える。

`carina-core/src/detail_rows.rs`
: `Effect::Replace` から detail rows を作る構造をやめ、`ReplaceDisplayMetadata` と対応する create/delete effect から replace rows を作る。`CascadingUpdates` row は削除し、consumer は通常 Update として表示する。

`carina-core/src/plan_tree.rs`
: raw effect tree とは別に、replace display metadata を見て Create/Delete pair を `+/-` 1 行へ畳む render item を追加する。DeferredReplace の既存表示は維持する。

`carina-core/src/value.rs`
: secret redaction の `Effect::Replace` arm を削除し、`UpdateBase` と `ReplaceDisplayMetadata` の redaction を追加する。Plan redaction は `replace_display` も通す。

`carina-cli/src/display/mod.rs`
: `Sigil::from_effect` から `Effect::Replace` を外す。plan tree rendering で `ReplaceDisplayMetadata` を使い、同一 binding の Create/Delete pair を `+/-` として表示する。consumer update は通常の `~` 行として出す。

`carina-cli/src/display/tests.rs`
: Replace snapshot を分解後の表示集約へ更新する。`+/-`、forces replacement、temporary name note、cascade ref hint、summary が維持されることを確認する。

`carina-cli/src/commands/apply/tests.rs`
: saved/apply fixture の `Effect::Replace` を分解済み effects に置き換える。旧 saved plan version rejection のテストも追加する。

`carina-cli/src/commands/apply/mod.rs`
: saved-plan load 境界は `run_apply_from_plan_with_observer_factory`。現在は `serde_json::from_str::<PlanFile>` 後に version check しているため、Effect::Replace 削除後は旧 plan が version check 前に serde 失敗する。ここを一度 `serde_json::Value` として読み、`version != 7` を先に検出してから `PlanFile` に deserialize する。

`carina-cli/src/commands/plan.rs`
: `PlanFile.version` を 6 から 7 に上げる。`PlanFile.plan: Plan` の中に `replace_display` が serde されるため、別 SavedPlan 型は作らない。

`carina-cli/src/commands/iam_preflight.rs`
: `Effect::Replace` arm を削除する。Create は create 権限、Update は update 権限、Delete は delete 権限として扱い、replace の IAM 要求は分解済み effects の合成で表す。

`carina-cli/src/tests.rs`
: `PlanFile` round-trip fixture に `replace_display` を含める。旧 version rejection と新 version round-trip を更新する。

`carina-cli/src/wiring/tests.rs`
: wiring fixture の通常 Replace を分解済み effects と metadata に置き換える。DeferredReplace のテストは維持する。

`carina-tui/src/app/mod.rs`
: `Effect::Replace` arm を削除し、replace display metadata から update/replace target set と tree node を作る。

`carina-tui/src/app/tests.rs`
: Replace fixture を Create/Delete pair と metadata に置き換える。selection、summary、detail pane が `+/-` 表示を維持することを確認する。

`carina-tui/src/ui/detail.rs`
: `CascadingUpdates` 表示を削除する。forces replacement と temporary name note は `ReplaceDisplayMetadata` 由来の detail rows で表示する。

## 主要設計

### UpdateBase を導入する

Stage 2 の冒頭で `Effect::Update` の `from` を次の型に変える。

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UpdateBase {
    Existing(Box<State>),
    CreatedBy { binding: String, id: ResourceId },
}

Effect::Update {
    id: ResourceId,
    from: UpdateBase,
    to: Resource,
    changed_attributes: Vec<String>,
}
```

通常 Update は `Existing(Box<State>)` を使う。temporary name の rename Update は plan 時点で from state が未確定なので、`CreatedBy { binding, id }` を使う。executor は `Existing` なら現在通り update request を作り、`CreatedBy` なら create 完了後に `ResolvedBindings` から同 binding/id の state を引いて update request を組み立てる。

別 `Effect::Rename` variant は採用しない。hidden operation を別 variant に移すだけになり、Update として scheduler に見せるという A-2 の目的から外れるため。

### Delete.blocked_by_updates を追加する

old Delete が consumer Update を待つ edge は、既存 `Delete.dependencies` には混ぜない。`dependencies` は「削除対象 resource が依存していた binding」であり、destroy 時の逆向き edge のための意味を持つ。consumer binding をここへ混ぜると、型上の意味が壊れる。

そのため `Effect::Delete` に apply 専用の `blocked_by_updates: HashSet<String>` を追加する。

```rust
Effect::Delete {
    id,
    identifier,
    directives,
    binding,
    dependencies,
    explicit_dependencies,
    #[serde(default)]
    blocked_by_updates: HashSet<String>,
}
```

`apply_edges()` は `dependencies` と `blocked_by_updates` の両方から `BlockedBy` を出す。`destroy_edges()` は `dependencies` のみから出す。これにより、CBD old Delete は consumer update 完了を待つが、destroy command の既存意味は変えない。

### temporary_name と rename

Create effect の Resource は実際に create する temporary name を持つ。`TemporaryName.can_rename=true` の場合、元名へ戻す操作は独立した `Effect::Update` として追加する。

```text
Create(to_with_temporary_name)
Update(from = CreatedBy { binding, id }, to = desired_original_name)
Delete(old, blocked_by_updates = consumers + optional rename binding)
```

rename Update 自身は Create に依存する。old Delete は provider の unique name 制約を避けるため、rename の位置に注意する。現行 executor は create -> consumer updates -> delete -> rename の順なので、Stage 2 でも delete 後 rename を基本にする。つまり old Delete の `blocked_by_updates` には consumer update binding を入れ、rename Update は old Delete に依存させる必要がある。これは rename Update の `to.directives.depends_on` または専用 schedule edge ではなく、通常の dependency model で表せるようにする。

### consumer Update の依存

`build_effect_dependency_analysis` は Apply 入力で resource-carrying effect の unresolved resource ref を見る。consumer Update の `to` が unresolved `web_acl.arn` を保持していれば、Update は new Create に依存する。

したがって必要条件は、consumer Update を独立 effect にすることと、saved-plan/live-apply 両方で `unresolved_resources` に pre-resolve Resource が入っていること。old Delete 側は `blocked_by_updates` で consumer Update binding を待つ。

### ReplaceDisplayMetadata と serde

`carina-cli/src/commands/plan.rs` の `PlanFile` は `plan: Plan` を持ち、`plan --out` は `PlanFile` を serde して保存する。したがって replace 表示情報は `Plan` 内に serde する。

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplaceDisplayMetadata {
    pub id: ResourceId,
    pub binding: Option<String>,
    pub create_idx: usize,
    pub delete_idx: usize,
    pub create_before_destroy: bool,
    pub changed_create_only: ChangedCreateOnly,
    #[serde(default)]
    pub cascade_ref_hints: Vec<(String, String)>,
    #[serde(default)]
    pub temporary_name: Option<TemporaryName>,
}
```

`Plan` に `#[serde(default)] replace_display: Vec<ReplaceDisplayMetadata>` を追加する。新形式 saved plan では必ず入れる。旧形式の自動 migration はしない。

### saved plan 互換とエラー

`PlanFile.version` を 7 に上げる。旧 saved plan は break 許容だが、低レベル serde error ではなく明示的な version error にする。

現在の境界は `carina-cli/src/commands/apply/mod.rs::run_apply_from_plan_with_observer_factory`。ここは現在、`serde_json::from_str::<PlanFile>` の後で `version != 6` を確認している。Effect::Replace を削除すると旧 plan はこの前段で失敗するため、次の順に変える。

1. `serde_json::from_str::<serde_json::Value>(&content)`
2. `value["version"]` を読み、`!= 7` なら「Unsupported plan file version ... Re-run carina plan」を返す
3. version が 7 の場合だけ `serde_json::from_value::<PlanFile>(value)`

これで旧 Replace を含む version 6 plan も、serde enum error ではなく再 plan を促すエラーになる。

### auto create_before_destroy の移行

Stage 1 では `Effect::Replace` が残るため、既存の `Plan::promote_to_create_before_destroy` を使い続ける。dependent scan で Replace を CBD に promote し、その後 consumer update を独立 effect として plan に出す。

Stage 2 では `PendingReplace` に `create_before_destroy: bool` を持たせる。dependent scan は `Plan` 上の Replace を mutate せず、PendingReplace の CBD/DBD フラグを確定する。plan に effects を追加するのは分解確定後だけにする。

### BlockedByIfDelete の削除

確認結果:

```text
carina-core/src/effect.rs:40:    BlockedByIfDelete(String),
carina-core/src/effect.rs:1043:                .map(ScheduleEdge::BlockedByIfDelete)
carina-core/src/effect/deps.rs:285:                ScheduleEdge::BlockedByIfDelete(binding) => {
```

`BlockedByIfDelete` は `Effect::Replace::apply_edges()` からしか生成されていない。通常 Replace を削除すれば生成元がなくなるため、Stage 4 で enum variant と resolver arm を削除する。

## 段階的実装プラン

### Stage 1: cascading update を二重実行なしで外へ出す

Stage 1 は二段に分ける。

1. Replace executor を先に変更し、`cascading_updates` を実行しない。`execute_cbd_replace_parallel` は create -> delete -> optional rename のみを実行する。これにより、以降で consumer Update を plan に追加しても二重実行にならない。
2. `cascade_dependent_updates` で、従来 `cascading_updates` に入れていた consumer を独立 `Effect::Update { from: Existing(...), ... }` として plan に追加する。既に Update がある場合は再利用する。
3. `Effect::Delete.blocked_by_updates` を導入し、CBD old Delete 相当の待ち先として consumer Update binding を入れる準備をする。ただし Stage 1 では Replace がまだ old delete を内包するため、Red test が Green にならない場合は Replace executor 内の delete 待ちを scheduler に出せていないことが原因になる。

Stage 1 の目的は、consumer update を hidden operation から外へ出し、二重実行しない状態を作ること。Red test がこの段階で Green になれば、Stage 2 以降は Replace 削除の cleanup に進める。Green にならない場合は、Replace 内包 delete が scheduler edge を持てないため Stage 2 を先に進める。

### Stage 2: UpdateBase と PendingReplace を入れて Replace を分解する

最初に `UpdateBase` を導入し、全既存 Update を `UpdateBase::Existing` に移行する。次に differ 内部へ `PendingReplace` を導入する。

`PendingReplace` は `id/from/to/directives/changed_create_only/temporary_name/cascade_ref_hints/create_before_destroy` を持つ。dependent scan は `PendingReplace` を更新し、CBD/DBD を確定する。最後に `decompose_replace_into_effects` が Create、consumer Update、old Delete、必要なら rename Update、`ReplaceDisplayMetadata` を生成する。

この Stage の完了条件は、production code から `Effect::Replace` construct が消えること。

### Stage 3: display 集約を復元する

`Plan.replace_display` を使って CLI/TUI/summary/detail rows を更新する。raw effects は Create/Update/Delete だが、operator 表示では同一 replace group を `+/-` 1 行に畳む。

summary は replace group を `to replace` として数え、group に含まれる Create/Delete を通常 create/delete として二重カウントしない。consumer Update は通常 update として数える。

### Stage 4: dead code 削除

`Effect::Replace`、`CascadingUpdate`、`cascading_updates`、`executor/replace.rs` の Replace 経路、phased Replace 専用 phase、`BlockedByIfDelete`、Replace 専用 detail/redaction/test fixture を削除する。`DeferredReplace` は残す。

## リスクと未確定事項

`force_replace` 系 directive が create-only schema diff と同じ理由型に乗っているかを確認する。create-only attribute 名がない replace reason がある場合、`ChangedCreateOnly` だけでは表示理由を表せないため、`ReplaceReason` 型へ広げる。

temporary rename の順序は provider 制約に直結する。現行の create -> delete -> rename を維持するなら、rename Update は old Delete 後に置く必要がある。Create -> rename -> delete へ変えると unique name conflict が再発し得る。

saved plan は version 7 で break する。migration はしないが、apply-from-plan の境界で必ず明示エラーを返す。

DeferredReplace は通常 replace metadata と summary/display で混ざりやすい。通常 replace group と DeferredReplace summary は別経路として扱う。

## テスト戦略

まず Red test の `carina-cli/tests/apply_cbd_consumer_ordering_e2e.rs` を Green にする。期待 op log は `create web_acl`、`update distribution`、`delete web_acl` の順。

unit では `decompose_replace_into_effects` を直接テストする。DBD、CBD、consumer update 追加、既存 consumer update 再利用、temporary name、rename Update、prevent_destroy error を分ける。

`carina-core/src/effect/deps.rs` には、分解済み effects で `Create -> Update -> Delete` が deps として現れるテストを追加する。Update の unresolved ref が Create に依存し、Delete の `blocked_by_updates` が Update に block されることを確認する。

`carina-core/src/differ/cascade_tests.rs` では、consumer に独立 Update がない場合は Update が新規追加されること、既に Update がある場合は再利用されること、create-only consumer は Create/Delete pair に promote されることを見る。

saved plan は version 6 の旧 plan が `run_apply_from_plan_with_observer_factory` で version error になること、version 7 plan が `Plan.replace_display` を round-trip することを確認する。

display は CLI と TUI の snapshot を更新する。execution effects は分解されていても、ユーザー表示は `+/-` 1 行、forces replacement、temporary name note、summary の `to replace` が維持されることを固定する。

verify は次を使う。

```bash
cargo check -p carina-core
cargo nextest run -p carina-core
cargo nextest run -p carina-cli
cargo nextest run -p carina-tui
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-*.sh
```
