# HCL/Terraform の限界と Carina の設計

**日付**: 2026-04-24
**形式**: 生成AI (Claude) との対話による探索
**性質**: 結論の確定ではなく、論点の整理と根拠確認。複数回の訂正を経た記録。

---

## ⚠️ この記録の精度について (重要)

このドキュメントは AI との対話形式の探索で、**精確性が保証されている記録ではない**。読む際は以下の前提で利用してほしい。

### 確認済みの論点

探索中にユーザーが疑問を投げた論点については、ソースコード・公式ドキュメントで検証して訂正している。具体的には:

- `terraform validate` の検出能力
- Terraform の plan プログラマビリティ (`terraform-json`, `terraform test` 等)
- Terraform の state 操作 (`moved`/`removed`/`import`) の実行パス
- cty の msgpack シリアライゼーション
- Carina の backend/arguments の文法と実装
- Carina のモジュール文法 (`use` 式)
- Carina の `AttributeType` と JSON 表現
- Carina の unknown 値の扱い (式木保持、resolver 挙動、`deferred_for_expressions`)

### 未検証の可能性が高いもの

それ以外の主張は**ソース確認を経ていない**。特に以下は注意:

- **CDK / Pulumi に関する記述**: AI の学習データ由来で、現在の実装と乖離している可能性
- **awscc のヒューリスティック補強の網羅度**: `resource_type_overrides` の実例は確認したが、全体像は未検証
- **OpenTofu の各機能の正確な範囲** (early evaluation, state 暗号化等): 公式情報を部分的に参照しただけ
- **Carina の `Custom` validator が LSP で実際に呼ばれる範囲**: CLAUDE.md の記述を根拠にしており、実装は未確認
- **「〜できる/できない」と書いた比較表の各項目**: ユーザー指摘のあった箇所以外は推測を含む
- **型理論の用語の精確な適用** (refinement type, nominal type 等): 型理論としての厳密さは未保証

### 推奨される読み方

- **論点の整理としては参考になる**が、個々の主張を無批判に引用するのは避ける
- 精確さが必要な論点は**個別に検証**してから使う
- **「どう訂正されてきたか」の記録 (§7 訂正履歴、§10 メタ観察)** を通じて、同種のバイアスが他の設計議論で再発しないようにする用途が主

### なぜ残すか

- 探索の**プロセス**自体に価値がある (結論よりも、どう論点が絞られていったか)
- AI との対話で頻発する**断定バイアス**の実例として記録する意味がある
- 未来の自分や他の人が、同じ論点を再探索する時の**出発点**になる

---

## 背景

「Terraform の限界は HCL に起因するものが結構ありそう」という仮説から出発し、HCL / CDK / OpenTofu / Carina の 4 ツールを比較しながら、インフラ管理ツールの言語設計を掘り下げた。

探索の過程で何度も断定的な主張が不正確と判明し、ソース確認と訂正を繰り返した。この記録は**結論よりも訂正プロセスに価値がある**ため、訂正履歴も残している。

---

## 1. 制約の出自を 4 層に分ける

Terraform の限界は単一原因ではなく、異なる層の制約が重なっている。これらを区別することが整理の出発点になる。

| 層 | 内容 | 解消手段 |
|---|---|---|
| **HCL/言語起因** | 型の表現力、参照モデル、モジュール境界 | DSL を作り直す (Carina) |
| **Terraformアーキテクチャ起因** | plan の扱い方、state 操作、schema 語彙 | 別実装で設計思想ごと変える |
| **組織的優先順位起因** | HashiCorp が触らなかった制約 | OpenTofu が Fork で解禁 |
| **本質的制約** | リソース作成後にしか決まらない値、ブートストラップ | どの設計でも残る |

※ 当初「商業/政治起因」と書いたが、HashiCorp が意図的に縛った証拠は一般公開されていないため「組織的優先順位起因」に訂正。

---

## 2. HCL/言語層の限界

精査の末、**純粋に HCL/言語起因**と呼べる限界は以下に絞られる。

### 2-1. sum type / union が表現できない

「A または B または C」という値の選択肢を型で表現する構築子がない。

```hcl
variable "auth" {
  type = object({
    password    = optional(string)
    token       = optional(string)
    certificate = optional(string)
  })
}
# 「どれか1つだけ埋める」は型で言えず、コメント+validationで補う
```

cty 型構築子に union がない。enum 相当は `string + OneOf validator` で表現するが、型としては `string` のまま。

**Carina の対応**: `AttributeType::Union { members }` と `StringEnum { values, namespace }` が schema の一級型。解消済み。

### 2-2. refinement type がない

既存型に述語を付けて「正の整数」「1-65535 の範囲」のような型を作れない。

```hcl
variable "port" {
  type = number
  validation {
    condition = var.port >= 1 && var.port <= 65535
    error_message = "Port must be 1-65535"
  }
}
```

validation は実行時述語で型と分離している。型としての再利用・合成・推論への反映がない。

**Carina の対応**: `Custom { base, validate, pattern, length, semantic_name, namespace }` が **nominal refinement type** (base + 実行時述語 + 名前) として働く。ただし静的検証 (Liquid Haskell のような SMT 証明) までは行かない。部分解消。

### 2-3. モジュールが値でない

Terraform の `module` ブロックは名前空間であって、値として変数に代入したり動的に選んだりできない。

```hcl
module "network" { source = "./modules/network" }

# module.network を値として扱えない
# locals { my_mod = module.network }   ← 不可
```

HCL の言語設計で「モジュールは設定の再利用単位」であって「値」ではない。

**Carina の対応**: `use { source = "..." }` 式がモジュール値を返し、`let` で束縛できる。呼び出し結果も値として扱える。

```
let network = use { source = './modules/network' }
let net = network { cidr_block = '10.0.0.0/16' }

awscc.ec2.SecurityGroup {
  vpc_id = net.vpc_id
}
```

少なくとも `let` 束縛可能な値という水準では一級化されている。ただし関数の引数/戻り値として渡せるか、データ構造に入れられるか、動的に選択できるかなどは現時点でユースケースが想定されておらず、**どこまで一級市民として機能するかは未検証**。

> **訂正 (2026-04-26 追記)**
>
> 上記 §2-3 の主張は、後続の検証 (公式ドキュメント・ソース確認) で **両者ともに部分的に不正確**と判明した。
>
> **Terraform 側の訂正**:
> `module.<NAME>` は実際には**値**である (公式ドキュメント [`expressions/references`](https://developer.hashicorp.com/terraform/language/expressions/references) で「`module.<MODULE NAME>` is a value representing the results of a `module` block」と明記)。`for_each` なしなら object、`for_each` 付きなら map of objects、`count` 付きなら list of objects。`locals { my_mod = module.network }` も書ける。
>
> 不正確なのは「値ではない」ではなく、**module の identity (どの `source` を instantiate するか) が静的にしか決められない**点。`source` 引数はリテラル文字列のみで、resource/data の出力で動的に選ぶことはできない (公式ドキュメント [`block/module`](https://developer.hashicorp.com/terraform/language/block/module) 明記)。OpenTofu 1.8+ の early evaluation でも static にしか解決できない (tfvars/env/リテラル由来の変数のみ)。
>
> したがって正確な定式化は「`module` は値だが、**module の identity は静的にしか決められない**」。
>
> **Carina 側の訂正**:
> 「`use { source = "..." }` 式がモジュール値を返す」という主張も、実装確認の結果**評価モデル上は値ではない**と判明した。`use` 式は `Value::Module` のような値型を持たず、`Value::String("${use:...}")` という参照文字列に変換され、モジュールレベルのメタデータ (`uses: Vec<UseStatement>`) として保持される。`let` 束縛だけが特別扱い (`parse_let_binding_rhs` 内で個別処理) されており、関数引数/list/map/動的選択/exports 等の他の位置に書いても評価器が `use_expr` を処理せずエラーになる (`carina-core/src/parser/mod.rs` の `parse_primary_value` に `use_expr` のマッチアームがない)。
>
> 当初は構文上 primary に含まれていて受理されるが評価で落ちるという grammar/evaluator のドリフト状態だったが、[carina#2233](https://github.com/carina-rs/carina/issues/2233) (2026-04-26 close) で **grammar の方を tighten する方向で解決**: `use_expr` の文法的位置自体が `let` 束縛 RHS に制限された。したがって現状の Carina module は「`let` 束縛変数経由の参照のみ可能」という形に整理されている。
>
> **比較の含意**:
> Terraform / Carina ともに「module を値として扱える範囲」と「module の identity を動的に決められる範囲」は別物で、両者をまとめて「値か否か」で語ったのが §2-3 の誤りだった。詳細な比較整理は別途 private な探索ノートで継続中。

### 2-4. unknown値 / 2パス評価 (言語+実行協調)

plan 時未知の値 (`(known after apply)`) の扱いが言語仕様として明示されず、ユーザーがエラーで学ぶ。

```hcl
resource "aws_s3_bucket_policy" "bar" {
  count = length(aws_s3_bucket.foo.*.id)
  # エラー: count value depends on resource attributes
  # that cannot be determined until apply
}
```

HCL 単独ではなく、Terraform 実行モデルとの協調問題。

#### Terraform の扱い

- unknown 値は cty.Value に**unknownフラグ**を立てて持ち運ぶ
- msgpack シリアライズ時は専用の ext type (`0xd4 0x00 0x00`)
- 伝播: unknown を含む式は原則 unknown を伝播 (例: `"prefix-${unknown}"` → unknown)
- `count`/`for_each` の値は graph 構築時点で確定必須 → unknown ならエラー
- Terraform 1.6+ の **refinements** で「unknown だが nonnull」「unknown だが長さ既知」等の部分情報を保持可能に (依存グラフベースの実行モデルの制約を型システム側で緩和する方向)

#### Carina の扱い (実装確認済み)

**`Value` enum に `Unknown` バリアントは存在しない** (`carina-core/src/resource.rs:173-216`)。代わりに **未解決の式を式木として保持** する設計:

```rust
pub enum Value {
    String, Int, Float, Bool, List, Map,
    ResourceRef { path },         // 未解決参照
    Interpolation(parts),         // 部分未解決の文字列
    FunctionCall { name, args },  // 部分未解決の関数呼び出し
    Secret, Closure,
}
```

resolver (`carina-core/src/resolver.rs`) の挙動 (実装確認):

- **`ResourceRef`**: state から解決できれば具体値に置換、できなければ Ref のまま (`return Ok(value.clone())`)
- **`Interpolation`**: 全パーツが解決できれば String に連結、未解決パーツが残れば `Interpolation` のまま
- **`FunctionCall`**: 全引数が解決できれば即評価、未解決引数が残れば `FunctionCall` のまま (部分評価)

`for` 式が未解決な iterable を持つ場合は `DeferredForExpression` として `deferred_for_expressions` に退避 (`carina-core/src/parser/mod.rs:531`)。`expand_deferred_for_expressions()` で **upstream_state (他プロジェクトの state)** から解決できれば展開、できなければ deferred のまま plan 表示に回される (`carina-cli/src/display.rs:248-251`)。これは Terraform の `count value depends on resource attributes...` エラーに対する Carina の答えで、**エラーにせず deferred として次のフェーズに送る**設計。

#### 構造的な違い: 「値 + フラグ」vs「式木そのまま」

| | Terraform | Carina |
|---|---|---|
| unknown の表現 | cty.Value + unknownフラグ | 未解決式木 (ResourceRef/Interpolation/FunctionCall) を保持 |
| 部分情報 | refinements (1.6+): nonnull/長さ/prefix 等 | 式木に含まれる情報そのまま (依存先・関数適用が値自体に残る) |
| unknown 値への関数適用 | unknown を伝播 | `FunctionCall` として式木保持 (遅延評価) |
| for/count に unknown | graph 構築エラー | `deferred_for_expressions` に退避、plan に「deferred」として表示 |
| シリアライゼーション | msgpack ext type | JSON の各 variant (`ResourceRef`, `Interpolation`, `FunctionCall`) |

Carina のアプローチは **partial evaluation / symbolic execution** に近い。式木をまるごと保持するので、後から解決できれば評価できる。refinements のような別機構は持たないが、式木自体が情報を保持するため必要性が下がる (トレードオフとしてシリアライゼーションは複雑化)。

#### 残る本質的な制約

- リソース作成前に決まらない値はどう足掻いても解決時まで unknown (物理的制約)
- deferred な for-expression を**同プロジェクト内の未 apply リソース出力**で解決できるか、apply 時にインクリメンタル展開されるかは**未確認**
- CDK/Pulumi との比較 (Token / `Output<T>`) についての記述は**ソース確認していない AI 出力**なので注意

### 2-5. 文字列補間の痕跡 (歴史的経緯)

HCL 0.11 以前は `"${aws_vpc.main.id}"` が必須。HCL2 で裸の式が可になったが、両方の書き方が併存。現代は `terraform fmt` で正規化され、実害は小さい。

---

## 3. Terraform アーキテクチャ起因の限界

### 3-1. plan のプログラマビリティ (粒度の違い)

当初「plan が外部プログラマブルでない」と書いたが、これは不正確だった。

**事実**:
- plan は JSON (`terraform show -json`) で外部プロセスに公開されている
- `terraform-json` Go ライブラリで型付きに読める
- `terraform test` でアサーション可能
- OPA/Sentinel/conftest でのポリシー検査エコシステムが成立

**本当の差**:
- tfplan バイナリは不可侵で、加工して apply する標準手段はない
- 同一プロセス内で型付き値として plan を加工する設計ではない

**Carina**: `Plan<Effect>` が型付き Rust 値で、内部コードが直接加工できる。`wiring.rs` の `add_state_block_effects()` が plan 生成後に state blocks を反映する設計。

### 3-2. validate の検出能力

当初「書き間違いが plan まで気づかれない」と書いたが誤り。調査により:

- `terraform validate` は provider plugin に `ValidateResourceConfig` RPC を呼び、**OneOf 等の validator まで実行する**
- enum 的な誤りも plan 前に検出可能

**Carina との差**: 検出できるかではなく、**仕組み**の差。

| | Terraform | Carina |
|---|---|---|
| enum 値の誤りを validate で検出 | ✓ (provider validator) | ✓ (型) |
| 検出の仕組み | 実行時述語 (OneOf) | 型システム |
| 型推論への反映 | string のまま | enum 型として伝播 |
| LSP 統合 | terraform-ls 依存 | 言語仕様の一部 |

### 3-3. state 操作 (moved/removed/import) の処理

当初「Terraform はバラバラに後付けで処理」と書いたが不正確。ソース調査の結果:

- **moved**: pre-plan で state を書き換え (`prePlanFindAndApplyMoves`)
- **removed**: plan graph 内の forget ノード
- **import**: plan graph 内の import ノード
- config 抽出レベルでは `internal/refactoring` package に一部集約

「後付けで雑」というより、**「plan 完成前に state/graph に吸収する」設計思想の帰結**。

**Carina との対比**:

| | Terraform | Carina |
|---|---|---|
| state blocks の反映タイミング | plan 生成の前/中 | plan 生成の後 |
| 差分計算エンジンが state blocks を知るか | 部分的に知る | 知らない |
| 実行パイプラインの統一度 | 分散 | 統一 (`add_state_block_effects`) |
| plan の可変性 | 不可侵 | 加工可能 (`Vec<Effect>`) |

### 3-4. schema 語彙と protocol

当初「protocol で型が潰れる」と書いたが、これも整理し直した。

**両者で共通**:
- Value 層は型タグなしで送られる (msgpack も JSON も)
- schema は別 RPC で事前共有 (`GetSchema` / `schemas`)
- 復元は schema 辞書と値の照合
- 復元の仕組み自体は同じ

**本当の差**: schema で運べる型の語彙。

| | Terraform | Carina |
|---|---|---|
| Schema の型語彙 | cty (string/number/list/map/object/tuple) | AttributeType (+ Union/StringEnum/Custom/Struct) |
| sum type を schema で表現 | × | ✓ (`Union`) |
| refinement を schema で表現 | validator 述語のみ | `Custom { base, pattern, length, validate }` |
| JSON の tagging | schema も untagged 寄り | schema は internally tagged (`"type": "union"` 等) |

つまり「protocol で型が潰れる」は両者共通で、差は schema で表現できる型の豊かさにある。

---

## 4. CDK / OpenTofu との比較

### CDK: 汎用言語系の光と影

| 得たもの | 失ったもの |
|---|---|
| 関数/クラス/ジェネリクスで自由に抽象化 | 意図の保存 (synth で CFN に落ちる) |
| npm、IDE、テストエコシステム | identity の安定性 (construct tree path からハッシュで論理 ID) |
| TS 型 | protocol 層 (CFN) で型が潰れる |
| - | Token (synth 時未知値の隠蔽) |

「コードを読めば分かる」を失った代償として抽象化の自由を得た。

### OpenTofu: Fork で緩められる範囲の実証

- Early variable evaluation (backend/module.source で variables 使用可)
- State 暗号化を言語組込
- Provider iteration

HCL/protocol の根本は動かしていない。Fork 元互換が進化の上限。
→ **組織的優先順位起因の制約は緩められるが、HCL/protocol 起因は残る**。

---

## 5. Carina の位置取り

HCL の宣言性を守りつつ、型を強くする第三の道。

### 現時点の設計判断

- **schema レベルの型語彙を豊かにする** (Union/StringEnum/Custom/Struct)
- **ユーザー値表現が型システムと統合** (namespaced enum identifier vs plain string)
- **provider 別に型供給のトレードオフ** (Smithy vs CFN+ヒューリスティック)
- **plan を型付き値として同一プロセス内で加工** (Differ 純粋 + wiring で意図反映)
- **state 操作を単一の加工関数に統一**
- **モジュールを少なくとも let 束縛可能な値として扱う**
- **宣言性を捨てない**

### 関数合成について

「関数合成は現時点では重視していない」が正確な表現。将来撤回して重視する方向に変える可能性は閉じていない。CDK のような抽象化を「取らない」と断定するのは誤り。

### provider の型供給

| provider | 供給源 | 型表現力 |
|---|---|---|
| aws | Smithy model | 強 (union/enum/sum type がネイティブ) |
| awscc | CFN spec + ヒューリスティック補強 | 中 |

awscc のヒューリスティック補強:
- CFN は `SubnetId` も `Name` も両方 `string` としか言わない
- codegen が property 名から専用型を推論 (`kms_key_arn`, `iam_role_arn` 等)
- `known_string_type_overrides` / `resource_type_overrides` 辞書
- 漏れはあるが主要な誤用パターンは捕捉

**コアの器が強いからこそ、供給側が弱くても補強で改善できる**。Terraform は器が弱いのでこの改善余地がない。

### 4 者の位置関係

3 クラスタで整理:
1. **HCL 系 (宣言性+弱い型)**: Terraform, OpenTofu
2. **汎用言語系 (強い表現力+意図喪失)**: CDK, Pulumi
3. **新 DSL 系 (宣言性+強い型)**: Carina

ただしクラスタの境界は硬いものではなく、Carina は若いプロジェクトゆえに位置取りが動的。モジュール値化のように CDK に似た要素を選択的に取り込む余地がある。

---

## 6. 探索中に起票した Issue

- **#2194**: リネーム自動検知 (Delete+Create を Move と認識、または `moved` ブロックをサジェスト)
- **#2198**: root configuration で `arguments` ブロックが silently 受理され backend から評価可能になる意図外の動作

---

## 7. 探索中に訂正した主張

断定的に述べた主張が、ユーザーの指摘または実装/ソース確認により不正確と判明した箇所の一覧。

| 当初の主張 | 訂正後 |
|---|---|
| Effect as value は Carina 独自 | Terraform も plan を値として持つ。差はプログラマブル層の有無 |
| HCL は型が外付け | Terraform にも variable type system あり。差は表現力と到達範囲 |
| Smithy が protocol 層を生き延びさせる | schema の器と provider 供給は別層。Smithy は供給源の一例 |
| awscc は CFN 由来なので型が潰れる | ヒューリスティック補強で部分改善 |
| plan が外部プログラマブルでない | JSON 公開・型付き lib・テスト fw あり。差は同一プロセス内粒度 |
| 商業/政治起因 | 根拠薄く、組織的優先順位起因と訂正 |
| Terraform は state blocks をバラバラに後付け | 実行は別経路だが、設計思想 (plan 完成前吸収) の帰結 |
| backend で arguments が使えるのは Carina の機能 | 意図外動作 (issue #2198) |
| 書き間違いは plan まで気づかれない | `terraform validate` が provider validator まで実行 |
| ユーザー空間と schema 空間が分離 | Carina も Terraform も同じ。差は schema 側の表現力 |
| Carina は schema とユーザー側が同じレイヤー | 属性型拡張不可は両者共通 |
| protocol で型が潰れる | Value 層では両者同じ。差は schema 表現力 |
| Terraform は state blocks をバラバラに後付け | 実行は別経路だが設計思想の帰結 |
| Carina は関数合成を捨てた | 現時点で重視していないだけ。将来撤回の可能性は残る |
| Carina の if 式で module_call は動的選択可能 | 文法上書けるが、実装・ユースケース未確定 |
| Carina は参照が型を持つ | Carina も言語単体では参照の型は決まらない。差は schema の豊かさ |
| HCL モジュール非一級は Carina も同じ | 現最新文法では use 式が値を返し、let 束縛可能。部分解消 |

---

## 8. 全体を貫く視点

最初の仮説「Terraform の限界は HCL に起因する」は**部分的に正しいが不十分**。精確には:

- **HCL/言語起因** (sum type/refinement なし、モジュール非一級) → DSL を作り直して解消 (Carina のアプローチ)
- **Terraform アーキテクチャ起因** (plan 不可侵、state 操作の分散パス、schema 語彙) → 別実装で設計思想ごと変更可能
- **組織的優先順位起因** → Fork で解禁可能 (OpenTofu が実証)
- **本質的制約** (実行時未知値、ブートストラップ) → どの設計でも残る

Carina の特徴を精確に言うと:

1. schema の型語彙を豊かにする (Union/StringEnum/Custom/Struct)
2. ユーザー値表現が型システムと統合
3. provider 別に型供給のトレードオフを設計
4. plan を型付き値として同一プロセス内で加工
5. state 操作を単一の加工関数に統一
6. モジュールを let 束縛可能な値として扱う
7. 宣言性を捨てない (関数合成は現時点で重視していないが将来の変更余地あり)

これらは **HCL/言語起因と Terraform アーキテクチャ起因の両方を同時に回避する設計**であり、OpenTofu では到達できない領域。

---

## 9. 残っている掘りどころ

探索中に出たが深掘りしきれなかった論点:

- unknown 値の扱い (Terraform `(known after apply)`, CDK `Token`, Carina の unknown 表現)
- state backend 設計思想 (外部化 / CFN 委譲 / 独自)
- 複数環境の扱い (workspaces / stages / ?)
- 原子性/途中失敗 (Effect as value で扱える余地)
- モジュール `use` の `source` に何が書けるか
- awscc のヒューリスティック補強の網羅性
- Custom validator の合成可能性 (refinement type としての成熟度)
- Carina のモジュールが一級市民としてどこまで機能するか (関数への受け渡し、データ構造への格納、動的選択等の検証)

---

## 10. 探索のメタ観察

この探索で繰り返し起きた認識の歪みとその原因:

1. **Carina との対比を際立たせたい誘惑**: 「Terraform は〜ない」「〜できない」と断定気味になる傾向
2. **文法を見て機能を断定**: 「文法で許されている」を「機能として使える」と拡大解釈
3. **ユーザー発言の過度な一般化**: 「そこまで重視してない」を「取らない、捨てる」と拡大解釈
4. **若いプロジェクトの可変性を軽視**: 現時点の優先順位を将来にわたる確定事項のように扱う
5. **用語の階層混同**: 「protocol」「schema」「value」「DSL」の層を区別せずに語る

これらに対しては:
- 断定する前にソース/ドキュメントで確認する
- Carina を褒めるために Terraform を不当に貶めない
- 「有無」ではなく「程度・粒度・仕組み」で差を語る
- 現在の状態と将来の可能性を分けて書く
- 層を明示してから差を語る

この探索が**繰り返しの訂正プロセス**として記録に値するのは、同種のバイアスが他の設計議論でも発生しうるため。**結論よりプロセスに学びがある**記録として残す。
