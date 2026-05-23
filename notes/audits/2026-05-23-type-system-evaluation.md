# Carina 型システムの評価 (2026-05-23)

一般的な型理論・PL 設計の観点から、carina-core の型システムを実装ベースで
評価したメモ。今後の改善方針 (Issue 化候補) もあわせて整理する。

## 1. carina の型システムを型理論用語で言うと

> **nominal な型同一性 (`TypeIdentity`) を基盤に、refinement types
> (`Custom { pattern, length, validate }`) と sum types (`Union`) を組み
> 合わせ、方向性のある assignability で検査する、健全性優先の
> 漸進的 (gradual) 型システム**。

主要構成要素 (`carina-core/src/schema/mod.rs`):

| 構成 | 種別 | 位置 |
| ---- | ---- | ---- |
| `String` / `Int` / `Float` / `Bool` / `Duration` | プリミティブ | `schema/mod.rs:383-` |
| `StringEnum { name, values, namespace, dsl_aliases }` | 有限値集合 (literal types) | `schema/mod.rs:409` |
| `Custom { identity, base, pattern, length, validate, ... }` | refinement type | `schema/mod.rs:416` |
| `List<T>` / `Map<K,V>` / `Struct { name, fields }` | 構造的コンストラクタ | `schema/mod.rs:445-461` |
| `Union(Vec<AttributeType>)` | sum type | `schema/mod.rs:463` |
| `TypeIdentity { provider, segments, kind }` | nominal 同一性キー | `schema/type_identity.rs` |

## 2. 構造的部分型はどこで使われているか

実装を読むと「純粋な構造的部分型システム」ではない。**構造的判定は
局所**で、骨格は nominal。

### 2.1 `is_assignable_to` は基本的に nominal (`schema/mod.rs:1396`)

`List<T>` / `Map<K,V>` / `Struct` どうしの代入可能性は、最終アームで

```rust
(a, b) => a.type_name() == b.type_name()
```

になっている。`Struct { name, .. }.type_name()` は `"Struct(name)"`
であり、**フィールド集合の構造比較ではなく名前一致**。つまり Struct の
代入可能性は nominal。`List<T>` も `"List<" + T.type_name() + ">"` の
文字列連結で、構造を見ているように見えて実態は nominal 名の合成。

例外は `Custom` の経路:
- `Custom→Custom`: `TypeIdentity::same_type` の per-axis 同一性 +
  `pattern` 文字列等価 + `length` 区間包含 (`narrow ⊆ wide`) +
  `base` への再帰。
- `Union`: sink が Union なら「いずれかに代入可能」、source が Union
  なら「全メンバ代入可能」。これは subtyping の join/meet 規則そのもの。
- `Int → Float` の一方向 widening。

### 2.2 `is_type_expr_compatible_with_schema` は構造的 (`validation/mod.rs:454`)

`let` 束縛や `ref` の型注釈 (`TypeExpr`) とスキーマ側 `AttributeType`
の整合性検査。ここは**真に構造的**:

- `TypeExpr::List(T)` vs `AttributeType::List` → 要素型を再帰 (`:548`)
- `TypeExpr::Map(T)` vs `AttributeType::Map` → 値型を再帰 (`:555`)
- `TypeExpr::Struct { fields }` vs `AttributeType::Struct` → フィールド
  集合の**双射照合** (`:570`、個数一致 + 各フィールド型を再帰)
- **`struct → map(T)` coercion** (`:582`): フィールド名や struct 名を問
  わず、全フィールドの型が `T` を満たせば map として通す。

最後の項目が、carina で唯一「構造的部分型らしい」コアース。Go interface
や TypeScript の structural typing に近い width subtyping の発想。

### 2.3 値検証 `validate_*` は構造的

- `validate_struct` (`schema/mod.rs:1221`): 必須フィールド存在検査、
  未知フィールドの拒否 + 似た名前 suggestion、各フィールドの再帰検査。
- `validate_list` / `validate_map`: 要素を再帰。
- `validate_union` (`:1289`): 全メンバ試行、`union_member_score` で
  構造的に最も近いメンバのエラーを返す (#2219)。

`validate_struct` は**未知フィールドを拒否**するので、width subtyping は
**負方向のみ**許す。これは Flow の `{| ... |}` exact types や Rust の
struct リテラルに近い「sealed」な扱い。

## 3. 一般的な観点からの評価

### 3.1 良い点

**(a) 健全性 > 完全性の姿勢**
- 正規表現言語包含は decidable でないので「文字列等価」で近似
  (`schema/mod.rs:1375-1395`)。undecidability を正面から認め、false
  positive (apply 時失敗) を避ける選択。
- `Custom { identity: Some(_) }` を受け手にして、`identity: None` の
  source は明示キャストでないと流せない (`schema/mod.rs:1417`)。
- IaC ドメインでは「クラウドで失敗するコスト > コンパイル時に厳しめに
  弾くコスト」なので、この姿勢は正解。

**(b) Refinement type の採用が本質的に正しい**
- ARN / VPC ID / Region をドメイン値として「`String` の精製」で表せる。
  Terraform HCL にはこの層がなく、ユーザミスが apply 時まで残る。
- Liquid Haskell / F* と同じ流儀。

**(c) `TypeIdentity` の per-axis 同一性**
- `provider + segments + kind` の三軸 + per-axis 包含 (`empty axis =
  wider`) は、**provenance を運ぶ nominal type** として整っている。
- `aws.iam.Role.Arn` と `aws.acm.Certificate.Arn` を区別し、generic
  `aws.Arn` は両方を受け入れる — これは IaC で実用的に効く設計。

**(d) Sum type (`Union`) の subsumption 規則が対称的**
- sink が Union → 「any」、source が Union → 「all」。subtyping の
  join/meet 規則そのもの。理論的に綺麗。

**(e) 方向性のある assignability**
- `Int → Float` の片方向 widening、refinement の `narrow ⊆ wide` 包含
  — どちらも教科書的。

### 3.2 微妙な点 (賛否ある設計判断)

**(f) Struct の代入可能性が nominal**
- 構造的部分型派 (Pierce, Cardelli) からは「width/depth subtyping を
  捨てている」と言われる。
- 一方 nominal 派 (Rust, Java) からは「provider 跨ぎで同名 Struct
  (`aws.s3.Bucket.Tag` vs `aws.ec2.Tag`) を取り違えない」と評価される。
- **IaC では nominal が正解寄り** — 偶然マッチ事故を避ける。

**(g) Width subtyping なし (未知フィールド拒否)**
- typo 検出には強いが、provider のスキーマ進化 (新フィールド追加) に
  弱い。実際 codegen で都度追従している。

**(h) Gradual typing の境界が曖昧**
- `Value::Deferred` / `Custom { identity: None }` / `Value::Unknown` /
  `TypeExpr::Unknown` が、それぞれ違う「未確定」を表している。
- Siek-Taha 流の `Dynamic` 型一本化に比べると把握しづらい。

### 3.3 弱点 (実害が出うる負債)

**(i) Subtyping lattice が形式化されていない**
- `is_assignable_to` は手書き match arm の集合。**反射性・推移性・
  反対称性**がコードでもテストでも保証されていない。
- 学術的な System F<: / DOT calculus なら judgement rules で記述され、
  推移性は公理。edge case で推移性が壊れる可能性がある。

**(j) `type_name()` 文字列で代入可能性を判定するのは "stringly typed"**
- `List<T>` の代入可能性を `"List<...>"` 文字列で比較する実装
  (`schema/mod.rs:1450`)。
- AST で構造的に再帰すべきで、文字列化は実装の都合 (hash / 表示用)。
- generics や higher-kinded types を入れた瞬間に破綻する。

**(k) Variance (共変・反変) が暗黙**
- `List<T>` / `Map<K,V>` の variance がコード上に明示されていない。
- 現状おそらく全部共変で動いているが、Java 配列共変と同じ unsoundness
  の罠を踏みうる (read-only なら OK、書き戻しがあれば NG)。
- IaC は基本 read-only なので実害は出ていないが、ドキュメント化なし。

**(l) 互換性判定の二重実装**
- `is_assignable_to` (AttributeType ↔ AttributeType, `schema/mod.rs`) と
  `is_type_expr_compatible_with_schema` (TypeExpr ↔ AttributeType,
  `validation/mod.rs`) が並立。
- 同じ「型は互換か?」を別ロジックで書いていて、片方を直して片方を
  直し忘れる事故が起きやすい。理論的には parse 後に一つの型表現に
  正規化するのが筋。

**(m) `validate_*` の "stringly typed" な分岐**
- `validate_concrete` のディスパッチは type_name 文字列を介する場面が
  ある。プリミティブの拡張 (新しい数値型など) で網羅性チェックが
  効きにくい。

## 4. 同時代の IaC 言語比較

| 言語 | 型システム | carina との距離 |
| ---- | --------- | --------------- |
| Terraform HCL | ほぼ untyped、providers がランタイム検査 | carina は世代が一つ進んでいる |
| Pulumi (TS) | TypeScript 構造的部分型、refinement なし | carina の方がドメイン適合度高い |
| Bicep | 構造的 + literal types | provider 軸 identity という発想は無い |
| Dhall | dependent-ish, 静的 | 理論的には最強だが実用ハードル高い。carina は中間点 |

## 5. 総合評価

**B+ 〜 A-**。

- **長所**: refinement types + per-axis nominal identity +
  sound-over-complete の姿勢は、IaC ドメインに対して 2026 年時点で
  ほぼ最適解に近い。
- **短所**: 形式化が弱い (subtyping lattice 公理化なし)、二重実装、
  `type_name()` 文字列化、variance 暗黙 — 実装が先行して理論が
  追いついていない状態。

PL 研究者の視点では「nominal vs structural の使い分けは意図的で評価
できる。formal semantics を書いて推移性・健全性を証明すれば論文に
なる素材」というレベル。

## 6. 改善候補 (Issue 化対象)

実害が出うる or 出始めているものを順に挙げる。番号は §3 と対応。

1. **(i) Subtyping lattice の公理化と性質テスト** — reflexivity /
   transitivity / antisymmetry を property test で担保。
2. **(j) `type_name()` 文字列比較から AST 構造比較への移行** —
   `is_assignable_to` の最終アームをコンストラクタごとの構造再帰に。
3. **(k) Variance の明示** — `List<T>` / `Map<K,V>` の variance を
   ドキュメント化、必要なら型に注釈。
4. **(l) `is_assignable_to` と `is_type_expr_compatible_with_schema`
   の統合** — 一方を他方の薄いラッパーに、または共通の正規化型に。
5. **(h) Gradual 未確定値の用語整理** — `Value::Deferred` /
   `Custom { identity: None }` / `Value::Unknown` / `TypeExpr::Unknown`
   の意味論を一枚にまとめ、可能なら一本化する道筋。
6. **(g) Width subtyping の限定的サポート** — provider スキーマ進化
   への耐性として、`Struct` に "open" モードを追加する設計を検討。
7. **(m) `validate_concrete` のディスパッチを type_name 文字列から
   コンストラクタ match へ** — 網羅性を Rust の match で担保。
