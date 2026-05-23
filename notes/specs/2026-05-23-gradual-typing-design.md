# Gradual Typing Long-Term Design (2026-05-23)

<!-- constrained-by ../audits/2026-05-23-type-system-evaluation.md#3-3 -->
<!-- derived-from ../audits/2026-05-23-type-system-evaluation.md -->

長期視点と型安全性を最優先にした、carina の「未確定値」設計のドキュメント。
今すぐ全部実装する設計書ではなく、**4 つの不変条件を Rust の型システムで
証明できる状態に到達することを目指す**ロードマップ。

## 1. 背景 — 現状の "Unknown" は何種類あるか

`notes/audits/2026-05-23-type-system-evaluation.md` §3.3 (h) の整理を
コードと突き合わせて精密化したもの。RFC #2972 (Phase 5) で `Value` の
Concrete/Deferred 軸は既に enum で分離されているので、Value 軸そのものは
型安全。問題は**異なる階層に同じ "Unknown" という名前で異なる意味の
未確定が散らばっている**こと。

### 1.1 形態の正確な分類

| # | 形 | 階層 | 意味 | 解決時期 |
| - | --- | ---- | ---- | -------- |
| 1 | `Value::Deferred(DeferredValue::ResourceRef / BindingRef / Interpolation / FunctionCall)` | 値 (式) | 参照や式 — 評価すれば concrete | **reference resolution 時** |
| 2 | `Value::Deferred(DeferredValue::Unknown(UpstreamRef / UpstreamBareRef))` | 値 (apply待ち) | upstream state 未解決、apply 後に判明 | **upstream apply 時** |
| 3 | `Value::Deferred(DeferredValue::Unknown(ForKey / ForIndex / ForValue / ForValuePath))` | 値 (式) | for-expression のループ変数、iterable が解決すれば置換可 | **iterable 解決時** |
| 4 | `Value::Deferred(DeferredValue::Unknown(EmptyInterpolation))` | 値 (パースエラー) | `${}` (空の interpolation) — 編集途中の sentinel | **(発生源で diagnostic) ** |
| 5 | `Value::Deferred(DeferredValue::Secret(_))` | 値 (orthogonal) | 値が秘匿対象、中身は (1)(2)(3) どれもありうる | (中身に依存) |
| 6 | `AttributeType::Custom { identity: None, .. }` | 型 (静的) | identity 軸で「区別しない」(3 つの意味が混在、§3 参照) | **(静的、不変)** |
| 7 | `TypeExpr::Unknown` | 型 (推論エラー復旧) | inference 失敗のエラー復旧 sentinel | **(静的、エラー復旧)** |

(1)(2)(3)(4) は **`DeferredValue::Unknown(UnknownReason)`** に同居しているが、
解決経路が違う:

- **(1) 参照式**は `resolve_ref_value` 系で reference resolution 時に解決。
- **(2) upstream**は別 carina スタックの apply 完了後に解決。
- **(3) for-loop**は iterable が解決した瞬間に `substitute_placeholder` で
  置換。
- **(4) `EmptyInterpolation`**は LSP diagnostic + 下流の tolerant 処理用。

(6)(7) は型レベルだが意味が異なる:

- **(6) `Custom { identity: None }`**は「軸を使わない=区別しない」(soundness バグの
  原因、carina#3218)。さらに **3 つの異なる "None" 意味**が混ざっている:
  - codegen synthesized anonymous(`identity を持たせる意義なし`)
  - パーサが生成した raw(`identity 解決中`)
  - 既知の bare 組み込み(`Ipv4Cidr`、`Email` 等、provider 軸なし)
- **(7) `TypeExpr::Unknown`**は inference 失敗の error recovery sentinel。
  正常系では決して残らず、ダウンストリームは `is_unknown()` で skip する。

### 1.2 既存の型安全レベル評価

| 階層 | 型安全性 | 評価 |
| ---- | -------- | ---- |
| `Value` の Concrete/Deferred 軸 | A | RFC #2972 で確立、borrow 型で経路保証 |
| `DeferredValue` の variant 別 | B+ | enum tag で区別可、ただし `as_deferred()` 経由は flat |
| `UnknownReason` の variant 別 | B | enum tag で区別可、消費側は match の網羅頼み |
| `TypeExpr::Unknown` の伝播 | C | コメントでしか役割が分からない |
| `Custom { identity: None }` の意味 | C | `Option<TypeIdentity>` の `None` に 3 つの意味 |
| **階層間の関係性** | D | `Value::Deferred::Unknown` ↔ `TypeExpr::Unknown` の対応が型で不存在 |

個々の階層は型安全だが、**階層をまたぐ未確定性の整合性は型で保証されていない**。
長期で問題になるのはこの層。

## 2. 設計原則

メモリ規範と RFC #2972 の流れから抽出した carina の型安全規範:

1. **不変条件は型システムで証明する、runtime チェックに逃さない**
2. **「ここでは X しか来ない」を Rust の型で表現できるなら、それを使う**
3. **境界(WIT / serde / provider plugin)以外では sentinel / `Option` / 文字列を避ける**

この規範を「未確定」周りに適用したのがこのドキュメントの提案。

## 3. 目指す姿 — 4 つの不変条件

### 不変条件 1: 解決時期を型で区別する

現状、`DeferredValue` は「reference resolution で解決」(BindingRef 等)と
「apply 時に解決」(Unknown::UpstreamRef 等) を**同じ階層に並べている**。
`resolve_ref_value` のような関数は「reference は解決できるが unknown は
素通し」というロジックを runtime match で書かなければならない。

**目指す形:**

```rust
pub enum Value {
    Concrete(ConcreteValue),
    Reference(ReferenceExpr),       // reference resolution で解決される
    ApplyTime(ApplyTimeUnknown),    // apply 時に解決される
    Secret(Box<Value>),             // orthogonal — 中身は任意の軸
}

pub enum ReferenceExpr {
    ResourceRef { path: AccessPath },
    BindingRef { binding: String },
    Interpolation(Vec<InterpolationPart>),
    FunctionCall { name: String, args: Vec<Value> },
}

pub enum ApplyTimeUnknown {
    UpstreamRef { path: AccessPath },
    UpstreamBareRef { binding: String },
    ForKey, ForIndex, ForValue,
    ForValuePath { path: AccessPath },
    EmptyInterpolation,
}
```

`ForKey / ForIndex / ForValue / ForValuePath` を `ApplyTimeUnknown` に
含めるかは**設計判断ポイント**。iterable 解決で置換される(=
"reference 解決時") とも見えるし、上流 apply 待ちと同じ「将来置換され
る placeholder」とも見える。Phase 1 で確定させる。

**型安全効果:** `validate` 関数が「reference は既に解決済み」を型で
要求できる。`writer` が「`Concrete` のみ」を型で要求できる。

### 不変条件 2: 型レベルの不在と値レベルの不在を統一する

`TypeExpr::Unknown`(型レベル)と `Value::Deferred::Unknown`(値レベル)
は名前が同じだが意味が違う:

- `TypeExpr::Unknown` = inference 失敗 = **エラー復旧 sentinel**(異常系)
- `Value::Deferred::Unknown` = 値が予測不能 = **正常な未確定状態**(正常系)

**目指す形:**

```rust
pub enum TypeExpr {
    String, Int, /* ... */
    // Unknown variant を消す
}

// 推論失敗は Option / Result で表現
pub type InferredType = Result<TypeExpr, InferenceError>;
```

または carina#3220 の `Never` 型を採用すれば、「推論失敗 = `Never`」と
すれば意味が綺麗 (`Never` はどの sink にも代入可能なので下流の型
チェックは"silently"進む — 副作用注意、要設計)。

**型安全効果:** `TypeExpr` を受け取る関数が「これは inference 成功した
型」を前提にできる。`TypeExpr::Unknown` を個別に分岐する 10+ 箇所が消える。

### 不変条件 3: `Custom { identity: None }` の "None" を意味で区別する

現状 `Option<TypeIdentity>` の `None` に 3 つの意味が混ざっている
(§1.1 (6))。

**目指す形:**

```rust
pub enum CustomTypeOrigin {
    /// codegen-synthesized refinement (pattern + length only).
    Anonymous,
    /// Provider-agnostic builtin (Ipv4Cidr, Email, ...).
    Builtin(TypeIdentity),
    /// Provider-attributed type.
    Provider(TypeIdentity),
}

pub enum AttributeType {
    // ...
    Custom {
        origin: CustomTypeOrigin,
        base: Box<AttributeType>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        namespace: Option<String>,
        to_dsl: Option<fn(&str) -> String>,
    },
}
```

**型安全効果:** carina#3218 の対称性バグを直すときに、「Anonymous source は
Provider sink に流せない」「Builtin source は Provider sink に流せる」
のような規則を**型レベルで分岐**できる。

### 不変条件 4: 解決段階を型相 (typestate) に持ち上げる

不変条件 1〜3 を統合すると、carina のあらゆる Value / Type / Expr に
**「解決段階」という同じ軸**が走っていることが見えてくる:

```
段階                値レベル                型レベル
─────────────────  ──────────────────────  ─────────────────
0. Raw             ReferenceExpr 含む       TypeExpr (推論前)
1. Inferred        ReferenceExpr 含む       TypeExpr / InferredType
2. ResolvedRefs    ApplyTimeUnknown 含む    AttributeType
3. Applied         Concrete のみ            AttributeType (final)
```

**目指す形:**

```rust
pub trait ResolutionStage { /* marker */ }
pub struct Raw;          impl ResolutionStage for Raw {}
pub struct Inferred;     impl ResolutionStage for Inferred {}
pub struct ResolvedRefs; impl ResolutionStage for ResolvedRefs {}
pub struct Applied;      impl ResolutionStage for Applied {}

pub struct StagedValue<S: ResolutionStage> {
    inner: Value,
    _phase: PhantomData<S>,
}
```

各段階移行関数:

```rust
impl StagedValue<Raw> {
    pub fn infer(self, ctx: &TypeContext) -> StagedValue<Inferred> { ... }
}
impl StagedValue<Inferred> {
    pub fn resolve_references(self, env: &RefEnv) -> StagedValue<ResolvedRefs> { ... }
}
impl StagedValue<ResolvedRefs> {
    pub fn apply(self, provider: &Provider) -> StagedValue<Applied> { ... }
}
```

シグネチャが段階を要求する:

```rust
pub fn validate(v: &StagedValue<Inferred>) -> Result<(), TypeError> { ... }
pub fn diff(a: &StagedValue<ResolvedRefs>, b: &StagedValue<ResolvedRefs>) { ... }
pub fn write_state(v: &StagedValue<Applied>) { ... }
```

これは Rust の **typestate pattern**。長期の carina 型システムの理想形。

**型安全効果:** Phase をまたぐ runtime check の 9 割が型レベルに昇格。
`unreachable!()` / `match { _ => false }` の "fallthrough false" 箇所が
構造的に消える。

## 4. 段階的導入ロードマップ

不変条件を一気に導入するのは無理。長期計画として 4 フェーズに分ける。

### Phase 1 (短期、1–2 か月): 用語と文書整理

**スコープ:**

- このドキュメント自体(用語整理、不変条件の宣言、ロードマップ)
- 階層ごとの "Unknown" 形態の表(§1.1)を `carina-core` のクレートドキュメント
  にも反映(`carina-core/src/lib.rs` か `resource/mod.rs` の module doc)
- `Value::Deferred::Unknown` と `TypeExpr::Unknown` の役割分担を `UnknownReason`
  の doc comment に明示

**コードは変えない**。設計を共有することが目的。

**完了基準:**
- このドキュメント merge 済み
- `carina-core/src/resource/mod.rs` と `parser/ast.rs` の module-level doc に
  リンクが入っている

### Phase 2 (中期、3–6 か月): 階層内の型安全強化

不変条件 2 と 3 の実装。Phase 2 は**互いに独立な 3 つの sub-issue** で
進められる。

**Phase 2.a — `TypeExpr::Unknown` 廃止 (不変条件 2)**

- `TypeExpr::Unknown` variant を削除
- inference 関数の戻り値を `Result<TypeExpr, InferenceError>` に変更
- consumers の `is_unknown()` skip ロジックを呼び出し側のエラーハンドリングに
  集約

影響: `carina-core/src/parser/`, `validation/`, `config_loader.rs` の
~10 箇所。

**Phase 2.b — `Custom { origin: CustomTypeOrigin }` (不変条件 3)**

- `AttributeType::Custom.identity: Option<TypeIdentity>` を
  `origin: CustomTypeOrigin` に置き換え
- carina#3218 の soundness 修正をこの新しい型に乗せて実装
  - Anonymous → Provider への代入を**型レベルで拒否**
  - Builtin → Provider は kind / segments 一致で許可
  - Provider → Provider は per-axis 包含
- WIT 境界 (`carina-provider-protocol`, `carina-plugin-host`) のシリアライ
  ゼーション更新

影響: 既存の `identity: None` 使用 ~15 箇所 + provider 側コード。

**Phase 2.c — `DeferredValue` 二階層化 (不変条件 1)**

- `Value::Deferred(DeferredValue)` を
  `Value::Reference(ReferenceExpr) | Value::ApplyTime(ApplyTimeUnknown) | Value::Secret(Box<Value>)`
  に置き換え
- `as_reference()` / `as_apply_time()` / `as_concrete()` の borrow projection
  を提供
- `resolve_ref_value` 系を `&ReferenceExpr` 引数に変更
- For-loop variants の所属を確定させる(§3 不変条件 1 の設計判断ポイント)

影響: 値の type-check が走るほぼ全箇所(数百ヶ所)。RFC #2972 と同等の
大規模 refactor。

### Phase 3 (長期、6–12 か月): 段階を型相に持ち上げる

不変条件 4 の実装。

- `StagedValue<S>` typestate 導入
- parser → inference → resolver → validator → differ → writer の各境界で
  段階を要求
- Phase 2 で残った runtime check を型レベルに昇格

これは epic タスク(複数 PR、複数開発者)。tracker issue で進捗管理。

### Phase 4 (超長期): formal semantics

- subtyping lattice の判定規則を judgement rules 形式で記述
  (carina#3208 の延長)
- property test で reflexivity / transitivity / antisymmetry を自動検証
- Phase 1-3 で得られた構造を ADR として固定

研究的タスク。short-term の実害は無いが、PL コミュニティとの対話に有用。

## 5. 型安全性の向上 KPI

長期視点で測定すべきメトリクス:

| 指標 | 現状の概算 | Phase 3 後の目標 |
| ---- | --------- | ----------------- |
| `unreachable!()` / `unwrap()` の使用箇所 | 多数 | 半減以下 |
| `match Value { ..., _ => false }` の "fallthrough false" | 多数 | ゼロを目指す |
| Phase をまたぐ runtime check 数 | 数百 | 数十 |
| public API で `Option<T>` の `None` が複数意味を持つ箇所 | 検出必要 | ゼロ |

Phase 1 完了時に baseline を計測(`scripts/count-runtime-guards.sh` の
ような計測スクリプトを書く)、各 Phase で改善を確認する。

## 6. オープン論点

### 6.1 `Never` 型 (carina#3220) との関係

不変条件 2 の inference 失敗を `Never` で表現する案は綺麗だが、
"silently 下流が進む" 副作用があり要設計。Phase 2.a 着手前に carina#3220
の Phase 1 を完了させて、`Never` の意味論を確定させてから決める。

### 6.2 typestate の serde / WIT 互換

`StagedValue<S>` を WIT 境界 / serde 越しに送れない。Phase 3 では
**境界では `Value` に erase、内部だけ `StagedValue<S>`** の二層構造に
なる見込み。境界変換関数で段階を assert する。

### 6.3 typestate のコンパイル時間影響

Rust の typestate は monomorphization が爆発する可能性。Phase 3 着手前
にプロトタイプで cargo build 時間 / バイナリサイズ影響を計測。

### 6.4 For-loop variants の所属

§3 不変条件 1 の「`ForKey / ForIndex / ForValue / ForValuePath` を
`ApplyTimeUnknown` に入れるか `ReferenceExpr` に入れるか」は
**Phase 2.c 着手前に決める必要がある**。判断基準は「いつ resolve され
るか」: iterable が解決した瞬間 (= reference resolution と同タイミング)
なら ReferenceExpr 側、apply 時の値解決と一緒なら ApplyTimeUnknown 側。
コードを読む限りは前者寄りに見える(substitute_placeholder は
reference resolution の一部)。

### 6.5 Phase 2 sub-issue の順序

Phase 2.a/2.b/2.c は独立に進められると書いたが、現実には依存がある:

- 2.b は carina#3218 の修正を含む。先に着手すべき。
- 2.c は範囲が最大。Phase 2 の最後でよい。
- 2.a は最小。いつでも着手可能。

推奨順: 2.b → 2.a → 2.c。

## 7. Cross-references

- `notes/audits/2026-05-23-type-system-evaluation.md` — このドキュメントの
  derived-from。型システム全体の評価と短期 Issue リスト。
- carina#3208 — subtyping lattice property tests (Phase 4 の伏線)
- carina#3212 — gradual typing "unknown" 形態整理 (Phase 1 のチケット)
- carina#3218 — `TypeIdentity::same_type` symmetric soundness バグ (Phase 2.b の前提)
- carina#3220 — `AttributeType::Any` / `Never` 導入 (Phase 2.a の選択肢)
- RFC #2972 — Value の Concrete/Deferred 分離 (このドキュメントの前提)
- RFC #2371 — `UnknownReason` 導入 (このドキュメントの前提)
