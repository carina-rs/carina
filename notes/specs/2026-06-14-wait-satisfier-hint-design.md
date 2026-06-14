# wait satisfier-hint — 設計

Issue: carina#3516 step (c)
親: #3497 / #3515 / #3522

## 解こうとしていること

#3497 / #3515 で wait fail-fast を入れて、#3522 で型レベルの barrier も入れた。
残るのは **wait の until が本当に「誰かに更新される」見込みかどうか** を plan 時に判定する仕組み。
現状はランタイム(executor)の「他に動ける effect が無い」検出に依存していて、
ユーザの depends_on 宣言にも依存している。これらは defense-in-depth として有効だが、
plan 時点で構造的に矛盾している wait を **plan 出力に出ない警告で済ます** か、
あるいは **DAG 依存エッジを足して dispatch 自体させない** ことができれば、
ランタイム検出に頼らなくてよい(根本対応)。

カリーナの依存グラフは **consumer 側**(`X が cert に依存する」)は持っているが、
**mutator 側**(「Y が cert.status を書き換える」)は持っていない。
ACM 証明書の status が `route53.RecordSet` の生成で flip する、というのは
provider 側の知識であって、DSL/schema には書かれていない。

## アプローチ: Provider::satisfier_hint

ユーザに `satisfied_by = [bindings]` を毎回書かせる(option 1)のは
convention-依存で型安全でない。長期的には provider が自分の mutation 関係を
**型のあるメソッド** で宣言する形が根本対応で、wait の依存補強は plan ステージが
自動でやる。

### 新 trait メソッド

```rust
impl Provider for ... {
    /// For a wait whose `until` predicate reads `attr_path` of the
    /// resource at `target_id`, return a list of binding-name patterns
    /// describing which neighbor effects could mutate that attribute.
    ///
    /// Returned patterns are matched against the plan's binding names at
    /// plan time. Each match contributes a dependency edge from the wait
    /// to the matched effect — so a wait whose satisfier set is empty or
    /// all-failed gets `EffectSkipped(unsatisfiable: dependency '...'
    /// failed)` at dispatch time instead of polling to timeout.
    ///
    /// Default impl returns `Vec::new()` — no hint, no auto-augmentation,
    /// fall back to the user's depends_on + runtime terminal check.
    /// Override to declare the mutator graph for resource types where
    /// the carina-side dependency graph cannot infer it.
    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> Vec<BindingPattern> {
        Vec::new()
    }
}
```

### BindingPattern

`BindingPattern` は plan 時に既知のバインディング名にマッチする型付きの述語。
具体的には:

```rust
pub enum BindingPattern {
    /// Exact match: this binding by name.
    Exact(String),

    /// "Any binding whose name starts with prefix `<base>[`, i.e. all
    /// expanded children of a for-loop binding `<base>`."
    ForLoopChildren { base: String },

    /// Type-and-attribute match: any binding of the given resource type
    /// whose attribute `attr` equals the resource's attribute `from`
    /// (referencing the target's own state). For example, ACM's
    /// satisfier hint is "any aws.route53.RecordSet whose name matches
    /// target.domain_validation_options[*].resource_record.name".
    AttributeMatch {
        resource_type: String,
        attr: AttrPath,
        from: AttrPath,
    },
}
```

最初の PR では `Exact` と `ForLoopChildren` のみ実装する。
`AttributeMatch` は plan ステージで state を見ないとマッチできない高度なケースで、
後続 PR で入れる。

### WIT

provider trait の追加は WASM プラグインにも対応が必要。

```wit
interface provider {
    enum binding-pattern-kind {
        exact,
        for-loop-children,
        attribute-match,
    }

    record binding-pattern {
        kind: binding-pattern-kind,
        base: option<string>,
        resource-type: option<string>,
        attr: option<list<string>>,
        from: option<list<string>>,
    }

    satisfier-hint: func(
        target-id: resource-id,
        attr-path: list<string>,
    ) -> list<binding-pattern>;
}
```

`carina-plugin-wit` の追加が必要。submodule の更新を伴う。

### plan ステージでの自動補強

`Effect::Wait` を構築する直前(differ で `WaitBinding` → `Effect::Wait`)に、

1. wait の `until` から `target_id` と `attr_path` を抽出
2. `provider.satisfier_hint(target_id, attr_path)` を呼ぶ
3. 返ってきた各 `BindingPattern` を plan 内の binding 名と照合してマッチング
4. マッチした binding 名を wait の `explicit_dependencies` に **追加**

ユーザが書いた `depends_on` は尊重する(消さない、追加するだけ)。

### 何が unrepresentable になるか

- **wait の satisfier set が空のままになる**: provider が hint を返さない場合、
  ユーザの `depends_on` のみが頼り。これは現状と同じ defense-in-depth。
- **provider が間違った hint を返す**: plan time に missing binding はエラーに、
  matched-but-irrelevant binding は wait を不要に block する。
  これは provider 実装のバグで、テストでカバーする。

## 1 PR スコープ (PR 1: foundation)

このコメントで作る最初の PR は:

1. `BindingPattern` 型を `carina-core/src/wait/mod.rs` に追加
2. `Provider::satisfier_hint` trait メソッドを追加 (default `Vec::new()`)
3. WIT に対応する型と関数を宣言 + carina-plugin-wit の PR を **先に出す**
4. `carina-plugin-host` の `wasm_factory.rs` に satisfier_hint の呼び出しを追加
5. 既存 mock provider (`carina-provider-mock`) は default impl で済む
6. テスト: trait method の default が空を返すことの確認

plan ステージの補強(差し込み logic)は **PR 2** に分離する。
理由: PR 1 は trait と WIT の追加で、provider 側 PR と同じ pattern (PR #3517 を真似る)。
PR 2 は plan ステージのロジックで、`BindingPattern` のマッチング実装 + wait の依存補強 + テストで independent scope。
これを 1 つに合わせると WASM プラグインの rebuild と plan の振る舞い変更を一度に行うことになり、debugging が難しくなる。

PR 3 では actual provider impl (carina-provider-aws の ACM など)。これは別 repo の作業。

## サブタスク

- Sub-issue A: WIT 拡張 (carina-plugin-wit に PR)
- Sub-issue B: carina-core trait + WIT 取り込み (この最初の PR、PR 1)
- Sub-issue C: plan ステージでの dependency augmentation (PR 2)
- Sub-issue D: actual provider impl (carina-provider-aws etc., PR 3)

## 検証

PR 1 単独では plan の挙動は変わらない。確認するのは:

- trait の default impl が動く (`cargo nextest`)
- WIT bindings が更新後コンパイルが通る
- 既存 mock / WASM provider impl がそのまま動く
- `Provider::satisfier_hint` の signature を直接呼ぶ unit test
