# AttrTypeKind::Custom 廃止 + Refined Primitive 型システム移行設計

<!-- derived-from ./2026-06-07-enum-state-coherence-design.md -->
<!-- constrained-by ./2026-06-07-unified-aws-type-identity-design.md -->

## ステータス

設計提案。日付は 2026-06-09。本ドキュメントは設計側で、実装 PR より
先にマージする必要がある（CLAUDE.md「Design PR must merge before
implementation PR」）。

動機の根 issue は carina#3427。Route53 HostedZone の `Name` が state
では末尾ドット付き、DSL では末尾ドット無しで表現され、plan が不要な
差分を出す問題である。この bug は本 chain の最終段階で、Route53 の
`Name` を refined String として表現し、読み戻し時の DSL 変換を同じ型
に載せることで同時に解決する。#3427 だけを直す独立 bug-fix PR は作ら
ない。

本設計は 2026-06-07 の enum state coherence 設計から、state 読み戻し
時の DSL 表現正規化を data-driven transform として扱う方針を引き継ぐ。
また unified AWS type identity 設計により、refined primitive が持つ
identity は aws/awscc の provider provenance ではなく AWS 上の値同一
性を表す。したがって String/Int/Float に `identity` を格上げするとき
も、同じ AWS 値を指す identity は provider を跨いで同一でなければな
らない。

## 問題設定

<!-- derived-from #ステータス -->

現状の `Custom` は、Arn、VpcId、FQDN、長さ制約付き文字列、数値範囲、
list item count など、primitive な値に追加の意味や検査を載せるための
外側 wrapper として導入された。`Enum` が DSL の shorthand と値集合を
扱うのに対し、`Custom` は構造化された値をそのまま validator に渡す型
として分かれている。

この分離は、構造型と enum shorthand を混同しないという点では有効だっ
た。しかし現在は、refinement metadata の置き場所として `Custom` だけ
が特別扱いされているため、複数の歪みが出ている。

第一に、`Custom` の DSL 変換は関数ポインタで表現されている。関数ポイ
ンタは WASM 境界を渡れないため、provider 側から構築された型は
host-side only の正規化を利用できない。結果として provider は、FQDN
や IP protocol alias のような「形式制約付き文字列の変換」を、値集合
を持たない pseudo-Enum として偽装して運ぶ道に落ちている。

第二に、その偽装は #3427 の直接原因になった。awscc#332 は
Route53 HostedZone の `Name` に末尾ドットを取り除く変換を載せたが、
それを運ぶために pseudo-Enum を使った。plan 比較では Enum arm が
dotted segment 抽出を行うため、`registry-dev.carina-rs.dev.` の末尾
ドットが空 segment として扱われ、DSL 側の
`registry-dev.carina-rs.dev` と一致しない。結果として意味的には同じ
HostedZone 名が `forces_replacement` として表示される。

第三に、`Custom` の `length` は文字列長だけでなく、Int の numeric
range としても使われている。実例は `max_session_duration`、`ttl`、
`ipv4_netmask_length`、`ipv6_netmask_length` などである。名前が
`length` のまま range を運ぶため、型の意味が値の種類に依存している。

根本原因は、pattern、length、validate、identity、to_dsl といった
refinement metadata を `Custom` という特殊 variant に集約しているこ
とである。各 primitive や List が自分の refinement を直接持てば、形式
制約付き文字列は String、数値範囲は Int/Float、item count は List に
自然に属する。変換も String の metadata として wire 越境可能な data
で持てるため、pseudo-Enum 偽装は不要になる。

## 提案する型システム最終形

<!-- derived-from #問題設定 -->

`Custom` variant を廃止し、refinement metadata を base 型へ格上げす
る。制約なしの primitive は、各 field が default 値の refined
primitive として表現する。これにより「制約なし String」と「FQDN のよ
うな制約付き String」は同じ variant 内で扱われる。

```rust
pub(crate) enum AttrTypeKind {
    String {
        identity: Option<TypeIdentity>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<DslTransform>,
    },
    Int {
        identity: Option<TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: CustomValidator,
    },
    Float {
        identity: Option<TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: CustomValidator,
    },
    Bool,
    Duration,
    Enum {
        identity: TypeIdentity,
        base: Box<AttributeType>,
        values: Option<Vec<String>>,
        dsl_aliases: Vec<(String, String)>,
        validate: Option<CustomValidator>,
        to_dsl: Option<DslTransform>,
    },
    List {
        element_type: Box<AttributeType>,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
    },
    Map {
        key: Box<AttributeType>,
        value: Box<AttributeType>,
    },
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
    Union(Vec<AttributeType>),
    Ref(String),
}
```

String の `pattern` と `length` は文字列の形と文字数を表す。`identity`
は Arn や ResourceId のような structured identity を表す。`to_dsl` は
state から DSL 表現へ戻す data-driven transform で、関数ポインタでは
なく `DslTransform` を使う。Route53 HostedZone の `Name` はここに
`length(max=1024)` と `StripSuffix(".")` を載せる。

Int の `range` は、現行 `Custom.length` に入っていた numeric range を
明示的に名前変更したもの。provider generated schema 本体では Int の
Custom が 114 件あり、そのうち 5 件が range を持つ。pattern と
to_dsl は実運用では使われていない。Float は awscc generated schema の
WAFv2 `size` で 5 件使われており、現在は validator のみだが、意味は
range validation であるため `range` field を持てる形にする。

List は item count refinement を持つ。awscc generated schema には
WAFv2 や SSO PermissionSet を中心に `Custom(base=List(...))` が多数あ
り、調査では parser 集計で 92 件、単純 grep では 110 件を確認した。
ここを primitive だけに移すと list item count 制約が消えるため、
List 自体に `length` と `validate` を持たせる。

List の要素型 field は、この chain で `inner` から `element_type` に
rename する。`inner` は List の文脈でも「何の内側か」が曖昧で、要素
型なのか wrapper の内側なのかを呼び出し側に読ませる。`element_type`
は意味が一意で、Map の `key`/`value` と同じく schema 上の役割を field
名だけで示せる。

rename 範囲は `AttrTypeKind::List`、`Shape::List`、`RawShape::List`、
`ProtoAttributeType::List` を揃える。Map の `key`/`value` は既に意味
が明確なので変えない。Enum の `base` も enum の underlying value shape
という意味が残るため変えない。PR1 で同時に実施し、`inner` と
`element_type` が混在する移行期間を作らない。

Bool と Duration は今回 metadata を追加しない。現状の provider 実利用
で `Custom(base=Bool)` と `Custom(base=Duration)` は 0 件と確認した。
前回の粗い分類で出ていた Other/Dynamic は、proto conversion の動的
base、test helper の `base` 変数、Arn helper の分類漏れを含んでいた
が、Bool/Duration は含んでいない。将来必要になった場合は、その時点で
Bool/Duration に意味のある refinement を追加する。

Map は現状維持とする。provider-aws と provider-awscc の generated
schema、carina repo 内の test/helper を含めて `Custom(base=Map(...))`
は 0 件と確認した。Map の key/value は既に型を持つため、今回の chain
では entry count など Map 自体の refinement は導入しない。

`AttributeType::custom(...)` は PR1 では互換 shim として残す。シグネ
チャは変えず、既存の 100 件超の呼び出しを同時に書き換えなくても、新
しい内部表現へ分配できるようにする。

```rust
// before
AttributeType::custom(identity, base, pattern, length, validate, to_dsl)
  -> AttrTypeKind::Custom { identity, base, pattern, length, validate, to_dsl }

// PR1 compatibility shim
AttributeType::custom(identity, base, pattern, length, validate, to_dsl)
  -> match base {
       String => AttrTypeKind::String { identity, pattern, length, validate, to_dsl },
       Int    => AttrTypeKind::Int { identity, range: length_as_range, validate },
       Float  => AttrTypeKind::Float { identity, range: None, validate },
       List { element_type, ordered } =>
           AttrTypeKind::List { element_type, ordered, length, validate },
       other  => migrate_or_reject_explicitly(other),
     }
```

PR5 で provider 側の移行が終わった後、この shim を削除し、呼び出し側
は refined constructor を直接使う形にする。

## Shape と RawShape API

<!-- derived-from #提案する型システム最終形 -->

`Shape::Custom` と `RawShape::Custom` は削除する。外部 crate は
`Custom.base` を見て再帰するのではなく、各 variant が直接返す
refinement metadata を読む。

最終形は概ね次のようになる。

```rust
pub enum Shape<'a> {
    String {
        identity: Option<&'a TypeIdentity>,
        pattern: Option<&'a str>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
        to_dsl: Option<&'a DslTransform>,
    },
    Int {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: &'a CustomValidator,
    },
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: &'a CustomValidator,
    },
    List {
        element_type: &'a AttributeType,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
    },
    /* remaining variants */
}
```

`RawShape` も同じ metadata を返す。ただし `RawShape` は従来通り Ref を
保存し、transport site が schema の cyclic structure を失わないように
する。

直接参照は 71 件ある。内訳は carina repo 33 件、provider-aws 24 件、
provider-awscc 14 件。典型的な書き換えは三つに分かれる。

一つ目は、`Custom` の base に delegate していた比較・walk site であ
る。これは String/Int/Float/List へ明示的に分岐する。

```rust
// before
Shape::Custom { base, .. } => walk(base)
Shape::List { inner, .. } => walk(inner)

// after
Shape::String { .. } | Shape::Int { .. } | Shape::Float { .. } => primitive()
Shape::List { element_type, .. } => walk(element_type)
```

二つ目は、identity を読む site である。これは各 refined variant から
直接取り出す。

```rust
// before
Shape::Custom { identity: Some(id), .. } => id

// after
Shape::String { identity: Some(id), .. }
| Shape::Int { identity: Some(id), .. }
| Shape::Float { identity: Some(id), .. } => id
```

三つ目は、transport conversion である。従来 `RawShape::Custom` を
wire の custom に戻していた site は、primitive/list の optional
metadata を持つ wire shape へ変換する。旧 wire 互換のため、移行期間中
は custom と refined primitive の両方を受け付ける。

## ProtoAttributeType の wire 表現

<!-- derived-from #shape-と-rawshape-api -->

`ProtoAttributeType::Custom` は当面残す。これは旧 provider から来る
payload を受ける deserialize compatibility layer として必要である。
ただし新しい provider は primitive/list variant に refinement fields
を載せて emit する。

primitive wire には optional fields を追加し、serde default を必須に
する。旧 payload が field を持たなくても、制約なし primitive として読
める必要がある。

`ProtoAttributeType::List` の要素型 field も `element_type` に揃える。
現行 wire が `inner` key を emit している場合、PR2 では deserialize 側
だけ `inner` を legacy alias として受け、emit は `element_type` に統
一する。これにより旧 provider payload を読める期間を残しながら、新し
い provider と docs では一つの field 名だけを使える。

`DslTransform` は wire 越境可能な data として扱う。既知の variant は
host が直接評価する。将来 provider が新しい transform variant を emit
し、古い host がそれを受け取った場合は `Unknown(serde_json::Value)`
として deserialize し、入力値をそのまま返す no-op に落とす。これは
forward compatibility のための明示的な設計判断であり、未知 variant を
panic や hard error にしない。

compatibility conversion は次の規則にする。

```text
proto::Custom { base=String, pattern, length, identity, to_dsl }
  -> refined String { pattern, length, identity, to_dsl }

proto::Custom { base=Int, length, identity }
  -> refined Int { range: length_as_range, identity }

proto::Custom { base=Float, identity }
  -> refined Float { identity }

proto::Custom { base=List(element_type), length, validate }
  -> refined List { element_type, length, validate }
```

ライフサイクルは段階的に進める。PR1 では core の refined primitive を
導入する。PR2 では protocol が旧 custom payload と新 primitive payload
の両方を読めるようにする。PR3 と PR4 で provider-awscc と provider-aws
が新型を emit する。PR5 で carina 側の custom emit を停止し、
deserialize compatibility を残すか削除するかを判断する。

互換性を保つため、`ProtoAttributeType::Custom` の削除はこの chain の
必須条件にしない。旧 provider が一定期間残る前提では、deserialize
compatibility を残す方が安全である。

## PR チェーン

<!-- derived-from #protoattributetype-の-wire-表現 -->

全体は五つの PR に分ける。PR5 を独立させる理由は、PR3 と PR4 の
provider 側 merge 後も、それぞれの provider pin が carina 新版を指す
までにラグがあり、旧 provider が `AttributeType::custom(...)` や
custom wire emit に依存している期間が残るためである。shim 削除と
custom emit 停止は、両 provider が refined primitive/list を emit す
る状態で安定してから行う。

### PR1: carina core refined primitives 導入

carina repo で `AttrTypeKind`、`Shape`、`RawShape` を変更する。
`AttributeType::custom(...)` は互換 shim として残し、base に応じて
refined String、Int、Float、List を返す。validate、type name、
assignability、differ、LSP の walk site は新しい shape に移行する。
同じ PR で List の `inner` field を `element_type` に rename し、
`AttrTypeKind`、`Shape`、`RawShape` の名前を揃える。

Acceptance は、`cargo check --workspace --all-targets` が clean である
こと、既存の schema、validation、differ、LSP tests が通ること、shim
経由で旧 API が引き続き動くこと。

### PR2: protocol compatibility

carina repo で protocol compatibility を実装する。`ProtoAttributeType`
は custom を deserialize 用に残し、custom payload を refined
primitive/list に変換する。primitive wire には optional refinement
fields を追加する。

Acceptance は、protocol serde roundtrip test、old custom payload の
deserialize compatibility test、新 primitive payload の roundtrip test
が通ること。List は old `inner` key を deserialize でき、新規 emit で
は `element_type` key を使うことも fixture で固定する。

### PR3: provider-awscc 移行

provider-awscc repo で carina pin を上げ、codegen を refined
primitive/list 出力に変更し、generated schema を再生成する。
Route53 HostedZone の `Name` は refined String として出し、
`length(max=1024)` と `to_dsl=StripSuffix(".")` を持たせる。

Acceptance は、carina#3427 の Route53 HostedZone plan snapshot が no
diff になること、awscc generated schema test と codegen test が通るこ
と。

### PR4: provider-aws 移行

provider-aws repo で carina pin を上げ、codegen と generated schema を
refined primitive/list に移行する。Arn、ResourceId、Int range 系の
generated schema を refined 形式にし、手書きの aws-types schema も同
じ形に揃える。

Acceptance は provider-aws tests が通ること、generated schema が refined
形式で出ることに加え、カテゴリごとの具体 fixture を確認すること。
Int range は IAM Role の `max_session_duration` と Route53 RecordSet
の `ttl`、refined String + identity は既存の
`tests/s3_bucket_data_source_arn.rs` と `tests/iam_role_arn.rs`、ResourceId
は EC2 VpcId と SubnetId を回帰防止対象にする。VpcId/SubnetId は
`src/schemas/generated/ec2/vpc.rs`、`subnet.rs`、`nat_gateway.rs` など
で使われる schema assertion を追加する。これらは `RawShape` または
provider schema assertion で、identity、pattern、range、validator が
refined variant から直接読めることを確認する。

### PR5: carina cleanup

carina repo で互換 shim を削除する。custom wire の emit は停止する。
deserialize compatibility は、旧 provider をどの期間受けるかに応じて
残すか判断する。PR3 で通した #3427 の regression を carina 側にも固定
し、将来 pseudo-Enum 偽装が復活したときに検知できるようにする。

Acceptance は、`cargo check --workspace --all-targets` が clean であ
ること、shim 削除後も全 tests が通ること、#3427 regression test が通
ること。#3427 の固定は `dynamic_enum_az_no_diff` のミラーとして、
`carina-cli/src/fixture_plan.rs` に
`route53_hosted_zone_name_strip_suffix_no_diff` fixture を追加し、
`carina-cli/src/plan_snapshot_tests.rs` に同名 snapshot test を追加す
る。state 側の HostedZone 名は末尾ドット付き、DSL 側は末尾ドット無し
にし、plan が no diff になることを確認する。

## PR 依存順序とタイミング

<!-- derived-from #pr-チェーン -->

PR1 と PR2 は carina repo 内で順番に merge する。PR2 の merge 後、
provider 側は carina pin を上げられるため、PR3 と PR4 は並列に進めら
れる。provider-awscc と provider-aws の両方が新型 emit に移行し、必要
な pin bump が終わった後に PR5 を carina repo で進める。

想定変更ファイル数は、carina core/lsp/plugin-host/protocol で 25 から
40 files、provider-aws で 15 から 30 files と generated schema、
provider-awscc で 20 から 40 files と generated schema 程度を見込む。
generated diff は provider schema の再生成量に依存するため、PR3 と
PR4 で実測する。

## リスクと未確定要素

<!-- derived-from #提案する型システム最終形 -->

Int の `length` を `range` に改名することで意味は明確になるが、既存の
validator 名や codegen template が `length` を前提にしている箇所を全
て移す必要がある。

List refinement は実在する。WAFv2 や SSO PermissionSet の item count
制約を落とさないよう、List 自体に `length` と `validate` を持たせる。
同時に `inner` という field 名を前提にしていた既存 codegen template、
provider conversion、test fixture をすべて `element_type` に移す必要
がある。

`Custom.base` に依存していた differ、union scoring、assignability は
dispatch 経路が変わる。これらは単に base に再帰するのではなく、refined
variant ごとの意味を保って比較する必要がある。

Enum の base は refined primitive になり得る。AZ や Region の
HyphenToUnderscore は引き続き Enum の `to_dsl` で扱うが、base が
refined String になっても state lift と differ が同じ意味を保つか確認
する。

protocol compatibility layer の寿命は別途判断する。旧 provider を一定
期間受けるなら deserialize compatibility を残す。全 provider が新型を
emit するようになった後も、古い lockfile や plugin payload を読む可能
性があるなら削除しない。

`to_dsl` は関数ポインタから `DslTransform` に変わる。これは単なる型変
更ではなく、WASM 境界を越える data-driven transform として意味を固定
する変更である。未知 transform を no-op として受ける方針に反して、
将来 hard error や panic を導入する場合は、旧 host と新 provider の
組み合わせを壊すため、別途 design review を必須にする。

aws と awscc の generated 再生成は diff が大きくなる。設計上は同種の
sibling site を一つの chain で扱う必要があるが、review しやすいように
provider repo ごとに PR を分ける。

## carina#3427 との関係

<!-- derived-from #問題設定 -->

carina#3427 の bug は、Route53 HostedZone の `Name` を refined String とし
て表現し、`to_dsl=StripSuffix(".")` をその String に直接載せることで
自然に解決される。pseudo-Enum は不要になるため、Enum arm の dotted
segment 抽出に巻き込まれない。

この修正は PR3 の provider-awscc 移行で入れる。PR5 では carina 側にも
regression test を固定し、Route53 HostedZone の state 名が末尾ドット
付き、DSL 名が末尾ドット無しでも no diff になることを確認する。

独立した #3427 専用の bug-fix PR は作らない。この design の chain が
root cause を扱い、最後の provider 移行で #3427 を同時解決する。

## 代替案

<!-- derived-from #問題設定 -->

選択肢 A は String に `to_dsl` だけを追加する案だった。これは
Route53 HostedZone の `Name` を plain String に戻す方向で、FQDN とい
う形式制約を型から落とす。意味論的に後退するため採用しない。

選択肢 B は `Custom.to_dsl` を `DslTransform` に変え、`Custom` variant
を残す案だった。#3427 は直るが、refinement metadata が特殊 variant に
閉じ込められる設計は残る。pseudo-Enum 偽装の直接原因は取り除けても、
型システムの歪みは温存される。

選択肢 C は Enum arm で transform 適用時だけ dotted segment 抽出をバ
イパスする案だった。これは問題の再発場所を一つ避けるだけの
band-aid であり、root-cause ではない。

選択肢 D は String と Custom の両方に transform を持たせる案だった。
同じ metadata の二重実装になり、新しい caller がどちらを使うべきかを
覚える必要がある。不要な複雑性を増やすため採用しない。

採用案は Custom 廃止と refined primitive/list への移行である。scope
は最大だが、型システムが意味論的に正しい形になり、将来の plain-string
transform、numeric range、list item count の利用者が同じ仕組みで救わ
れる。

## 実測値

<!-- derived-from #pr-チェーン -->

調査時点の clean baseline は `cargo check --workspace --all-targets`
で errors=0。

Custom 廃止と refined primitive/list の近似 stub を入れた実測では、
carina-core 内で止まった時点で errors=140。内訳は `Custom` variant 削
除に伴う E0599 が 51、unit variant だった String/Int/Float が struct
variant になった E0533 が 73、List に field を追加した E0027 が 6。こ
れは core 内下限であり、外部 crate まで到達するには core の walk site
移行が必要である。

`Shape::Custom` と `RawShape::Custom` の直接参照は 71 件。内訳は
carina repo 33 件、provider-aws 24 件、provider-awscc 14 件。
`ProtoAttributeType::Custom` 関連参照は 24 件。

`AttributeType::custom(...)` の base 別利用は、String 344 件、Int 124
件、Float 9 件、Nested Custom 55 件を確認した。generated schema 本体
では Int は range validation、Float は WAFv2 size の range validation、
List は item count validation として使われている。Int の pattern や
to_dsl は実運用では確認していない。

追加確認として、`Custom(base=Bool)`、`Custom(base=Duration)`、
`Custom(base=Map(...))` はいずれも 0 件だった。前回の Other/Dynamic
分類は proto conversion、test helper の動的 base、Arn helper の分類漏
れであり、Bool/Duration/Map refinement は含まれていない。

## まとめ

<!-- derived-from #問題設定 -->
<!-- derived-from #提案する型システム最終形 -->
<!-- derived-from #pr-チェーン -->

`Custom` は、refinement metadata の置き場所として広すぎる役割を負っ
ている。String の形式制約、Int/Float の range、List の item count、
state から DSL への変換を一つの wrapper variant に集約した結果、
provider wire、differ、LSP、assignability が base delegate に依存し、
carina#3427 のような pseudo-Enum 偽装を生んだ。

本設計は `Custom` を削除し、refinement を実際の base 型へ格上げする。
これにより、Route53 HostedZone の `Name` は refined String として型付
けされ、末尾ドット除去 transform も同じ String metadata として運ばれ
る。数値 range と list item count も同じ chain で意味に沿った名前と
場所へ移す。

実装は五つの PR に分け、design merge 後に core、protocol、awscc、aws、
cleanup の順で進める。#3427 は awscc 移行 PR で同時解決し、cleanup PR
で regression test を固定する。
