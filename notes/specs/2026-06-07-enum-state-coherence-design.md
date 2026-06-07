# Enum 属性の整合性: `StringEnum`/`CustomEnum` 統合と state 読み戻し正規化 (carina#3409)

<!-- derived-from ./2026-05-31-enum-identity-intermediate-segments-design.md -->

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。
本 issue (carina#3409) の Acceptance のうち「documented contract」を
本 PR で満たし、enforcement と回帰テストは後続実装 PR で扱う。

## 課題 (carina#3409)

ある provider が属性を素の `String` から namespaced enum に「移行」
した後、その属性を以前から保持している既存インフラを `plan` で再評
価すると、本来ゼロバイトの差分のはずが破壊的な `forces_replacement`
として表示される。発端は `validate` も `apply` も成功するのに、状態
保存時の正規化と読み戻し時の正規化が食い違っていることにある。

issue 本文の repro は `awscc.ec2.Subnet.availability_zone`。`.crn` 側は
`'ap-northeast-1a'` のままで、`apply` 後の `plan` が

```
availability_zone: "ap-northeast-1a" → "ap_northeast_1a" (forces replacement)
```

を 3 つの Subnet について報告し、依存する NAT Gateway / Route Table
Association を連鎖的に置き換えようとする。実際には AZ も VPC も CIDR
も変わっていない。同種の現象は同じ contract が破れているかぎり、
今後 enum 化される任意の属性（Region、instance type、lifecycle stage、
…）で再生産される。

issue は四つの取りうる挙動を整理し、現状の「書き込みは正規化済み、
読み戻しはバイトそのまま、差分が永久に出る」を **(4)** と呼んで明示
的に禁止することを求めている。残る三案 (1) String 一貫、(2) validate
拒否、(3) 双方向正規化のうち、本設計は **(3) を Carina-side で強制**
する道を採る。理由は次節で述べる。

## 採用する contract: (3) 双方向正規化を Carina-side で強制

属性が namespaced enum 形（`StringEnum` または `CustomEnum`）であるかぎり、

- `validate`/`plan` 入力 (`.crn`) は DSL spelling（`ap_northeast_1a`、
  `allow`、…）と API spelling（`ap-northeast-1a`、`Allow`、…）の両方を
  受け付ける。
- 受け付けた値は内部表現として **DSL spelling の `EnumIdentifier`**
  に正規化される。
- `apply` で state に書く値も `EnumIdentifier`（DSL spelling）として
  保存される。
- 既に書かれていた state を読み戻すときは、生 `String` で来た値も
  内部で `EnumIdentifier`（DSL spelling）に持ち上げる（lift する）。
- 結果として differ に到達するときには、desired と state の両側が
  必ず同じ表現になる。

他の二案を採らない理由:

- **(1) String 一貫** は phase 4 (carina#2986) と逆行する。namespaced
  enum の型情報を捨てる方向で、`validate` の品質が後退する。
- **(2) validate 拒否** は「過去のデプロイ済みインフラが永久に手作業
  での移行を要求し続ける」状態を残す。 CLAUDE.md の「new caller
  tomorrow」テストに反する。型ではなく運用で覚えておく必要が出る。

(4) を「禁止」とだけ宣言しても provider 著者が間違ってそこに落ちる
道は塞がらない。そこで本設計では **broken state を表現不可能にする
型レベルの強制** を contract と同時に導入する。

## 根本原因と統合の必要性

`carina-core` の現状の `AttrTypeKind` は、enum 系を二つの別 variant で
保持している（`carina-core/src/schema/mod.rs:493 / 551`）。

- `StringEnum { name, values, identity, dsl_aliases }` は値集合と
  DSL ↔ API の書き換えマップをデータで保持する。codegen が静的に
  列挙できる enum（CFN/Smithy のクローズドな enum）で使われる。値集
  合と書き換え表が data であるおかげで WASM 越境ができる。
- `CustomEnum { identity, base, validate, to_dsl }` は値集合を関数
  `validate` の中に閉じ込め、DSL 書き戻しは `to_dsl: Option<fn>` で持
  つ。値集合が動的・巨大・regex で定義しやすいような enum（AZ、
  Region、instance type、…）で使われる。関数で持つので host-side only。

二つの variant が同じ役目（namespaced enum）の二通りの実装で、
state 読み戻し時の正規化に必要な情報の在処が違うために、enforcement
を二度書く必要が出ている。実際 awscc#251 の修正
(`lift_string_enum_leaves` 一式、`carina-core/src/utils.rs:967` 以降)
は `StringEnum` だけを対象にしており、Plan/Apply/State 三経路の wiring
（`carina-cli/src/wiring/mod.rs:1615` / `:1691` / `:2041`、
`commands/apply/mod.rs:887`, `commands/state.rs:1101`）はすべて
`StringEnum` 側にしか効かない。`CustomEnum` の AZ は素通しで、これが
issue 本文の Subnet repro の直接の原因である。

「`CustomEnum` 側にも姉妹 lift を一本足す」のは、複数の consumer
サイト（lift と wiring）に同じ作法を二系統並べる per-class カーブア
ウトであって、CLAUDE.md の「複数の consumer サイトに `resolve_*` を
散らすのは type を直すべきサイン」に該当する。新しい schema 著者が
今後 enum を `StringEnum` で書くか `CustomEnum` で書くかを正しく覚
えていないと (4) を再生産する。**型のシグネチャだけで答えなければな
らない**。

したがって設計の本筋は二つを束ねた単一型 **`Enum`** に統合し、
state 読み戻し正規化を Enum 型一種類だけに掛けることである。

## 統合後の型形状 (`Enum`)

`AttrTypeKind` に新 variant `Enum` を追加し、`StringEnum`/`CustomEnum`
を削除する。

```rust
Enum {
    identity: TypeIdentity,
    /// 統合元 StringEnum の values。CustomEnum 出自では None。
    /// data 形で持てる場合のみ詰める。WASM 越境はこの field のみ。
    values: Option<Vec<String>>,
    /// 統合元 StringEnum の dsl_aliases。CustomEnum 出自では空。
    dsl_aliases: Vec<(String, String)>,
    /// 統合元 CustomEnum の validate / StringEnum なら `validate_in_values`。
    /// host-side で動く。WASM 越境では No-op に縮退。
    validate: CustomValidator,
    /// API spelling → DSL spelling の書き戻し関数。
    /// `dsl_aliases` を data から導けるなら自動合成、なければ provider
    /// が登録した関数を使う。state 読み戻し正規化はここを必ず通る。
    to_dsl: Option<fn(&str) -> String>,
}
```

統合方針の要点:

- `identity` は両者で必須化する（旧 `StringEnum.identity: Option` の
  `None` ケースは legacy の built-in shape だが、現状の provider crate
  での出現箇所はゼロに近い。統合時に `identity` 必須にしてしまい、
  legacy で `None` を必要としていた極少数の構築点は識別子を補う方向
  で書き換える）。
- `values` は `Option` のまま残す。codegen が enum を静的に列挙でき
  るときは詰める（旧 `StringEnum` 経路）、できないときは `None`
  （旧 `CustomEnum` 経路）。`Option` で持つことで data validation
  の最適化と関数 validation の両方が同じ型で表現できる。
- `dsl_aliases` は data 形で持ち、`Vec` のまま残す。旧 `CustomEnum`
  出自では空 `Vec` で構築され、書き戻しは `to_dsl` の関数経由になる。
- `validate` は両者の validate を統合する。旧 `StringEnum` 出自では
  「`values`/`dsl_aliases` に含まれているか」を見る自動合成版を
  入れる。旧 `CustomEnum` 出自ではそのまま渡す。
- `to_dsl` は `Option` のままにする。旧 `StringEnum` 出自では
  `dsl_aliases` から「API → DSL」の正引きを自動合成、旧 `CustomEnum`
  出自では既存の `to_dsl` を渡す。

これにより、Carina-core / LSP / differ / state 読み戻し経路は **Enum
型を 1 つだけ** 知っていれば良い。WASM 越境部 (`Shape` / `RawShape`)
には現状 `StringEnum`/`CustomEnum` が並列に存在しており、これも
`Enum` 一本に集約される。

`Custom` は今回統合しない。`Custom` は ARN / VpcId などの structural
pattern 専用で enum-shorthand を持たない別系統（carina#3222 で意図的
に enum と分離された）。今回の統合はあくまで「enum-shorthand 系」を
二型から一型にする話で、structural pattern との分離は維持する。

## enforcement の場所

統合後、state 読み戻し正規化は `carina-core/src/utils.rs` の
`lift_state_string_enums_to_identifiers` を `lift_state_enum_leaves`
（仮）に改名し、Enum 型をリーフ判定する単一経路に統一する。Plan/
Apply/State の三経路の wiring 自体は既に揃っているので、それぞれの
呼び先関数名を新名に差し替えるだけで通る。

合わせて、`canonicalize_with_type`（`carina-core/src/value.rs:1380`）
の match に Enum arm を追加する。これは現在 `string_or_list_of_strings`
専用に偏った正規化を行っており、Enum 型の leaf に対する正規化が
分散している状態を、`canonicalize_with_type` 側にも明示する。
desired 側と state 側で同じ canonicalizer を通す重要な不変条件で、
`union_member_score`/`select_union_member` の前例
(carina#3080) と同じ「ranker は一本、その出力で枝を選ぶ」の方針を
踏襲する。

## 既存 per-attribute fix との関係

awscc#250 / carina#3053 / awscc#249（IAM policy document の
`version` / `effect`）は、`AttrTypeKind::Custom` を `AttrTypeKind::
StringEnum` へ昇格させ、`resolve_enum_value` を `EnumIdentifier` 入力
にも効くようにした上で、`lift_*_string_enums` を 3 経路に wiring した
一連の作業である。awscc#251 はその範囲を「state に String で残って
いる過去値」まで広げ、 issue 本文が「StringEnum 側はこれで揃った」
と認識している到達点。

本設計はその到達点を **enum 一般** に拡張する。具体的には:

- 統合型 `Enum` 1 本に集約することで、`StringEnum` 専用の lift と
  `CustomEnum` 専用の to_dsl が二度書きされていた現状を 1 経路化する。
- enforcement の seam は変わらず（wiring の 5 ヶ所はそのまま）、
  関数名と入口型だけが差し替わる。

awscc#251 のクローズ条件は、本 contract の implementation PR が
merge され、IAM `version`/`effect` を含む既存 state が DSL spelling
の `EnumIdentifier` に lift されることで満たされる（awscc#251 は別
PR でクローズする必要はなく、本 implementation PR が一括で解決する）。

## 回帰テストの形

実装 PR で次の二系統を最低限揃える。本設計 PR では「形」だけを示し、
実テストは実装 PR で書く。

- Acceptance: 「`String` → namespaced enum 移行」を一つの真のシナリオ
  で再現する fixture。issue 本文の Subnet repro
  (`awscc.ec2.Subnet.availability_zone`) を題材に、state に
  `"ap-northeast-1a"` を含む JSON state を用意し、`carina plan` を
  fixture モードで走らせて差分ゼロを assert する。これは `CustomEnum`
  出自の Enum を確実に踏む。
- Unit: `StringEnum` 出自 (`aws.iam.PolicyDocument.Effect`、
  `awscc.ec2.Subnet.HostnameType` のように既に StringEnum 化されている
  もの) と `CustomEnum` 出自 (`aws.AvailabilityZone.ZoneName`、
  `aws.Region`) の両方について、state の生 `String` 値が lift で
  `EnumIdentifier` に変わることを直接 assert する。awscc#251 の既存
  test `lift_state_string_enums_to_identifiers_fixes_awscc251`
  (`carina-core/src/schema/tests.rs:5354`) と同じ枠で、
  `CustomEnum` 出自分も対称に並べる。

差分が出てしまう側を assert するだけでなく、未知の文字列（valid
values にも dsl_aliases にも一致しない）は lift せず素通しすること
（`validate` がそこで genuinely-invalid を弾くため）も unit で
assert する。

## 適用順と PR チェーン

本ドキュメント PR が merge された後、implementation PR を一本立てる。
工程の中身は型統合（`AttrTypeKind` の variant 差し替え）→ Carina-core
内の経路集約（lift / canonicalize の関数名・入口型差し替え）→ provider
crate (`carina-provider-aws` / `carina-provider-awscc`) の構築点を
新型に揃える → 回帰テストの順。一 PR で揃えるのは、二型存在を中途
半端に温存した中間状態（half-migrated）を残さないため。

実装 PR は本ドキュメントに `<!-- supersedes -->` で旧 `StringEnum` /
`CustomEnum` を扱う設計ドキュメント節を吸収する位置に立つ。
