# Schema 駆動の enum value 抽出設計 (carina-provider-aws#423)

<!-- derived-from ./2026-05-31-enum-identity-intermediate-segments-design.md -->
<!-- constrained-by ./2026-06-07-enum-state-coherence-design.md -->

## ステータス

Design proposal. Cross-repo chain (carina → carina-provider-aws →
carina-provider-awscc). Must merge before any implementation PR
(CLAUDE.md "Design PR must merge before implementation PR").

## 問題

carina-provider-aws#423 の再現点は、acceptance test
`acceptance-tests/tag_deletion/ec2_subnet` の step2 である。refresh 後の
state は Subnet の nested enum を 6-segment の structural identity として
保持している。

```text
aws.ec2.Subnet.PrivateDnsNameOptionsOnLaunch.HostnameType.ip_name
```

plan 表示ではこの値が `"HostnameType.ip_name"` に短縮され、その後 apply
時に `ec2.ModifySubnetAttribute` へ流れる。AWS SDK に渡る直前の
provider 側では、`carina-provider-aws/carina-provider-aws/src/services/ec2/subnet.rs:203`
が次の処理をしている。

```rust
let hostname_val = convert_enum_value(ht).replace('_', "-");
```

ここで期待される wire value は `ip-name` だが、実際には
`convert_enum_value` が `HostnameType.ip_name` を返し、provider 側の
AWS spelling 変換で `HostnameType.ip-name` になる。AWS は
`private-dns-hostname-type-on-launch must be one of ip-name, resource-name`
として `400 InvalidParameterValue` を返す。

つまり問題は `_` と `-` の置換ではない。置換の入力として渡される enum
value の切り出しが、schema なしの位置決めに依存していることが根である。

## プロデューサ/コンシューマの乖離

carina#3378 / #3383 / #3385 で producer 側は変わった。codegen と
hand-written identity は中間 struct segment を落とさず、
`aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled` や
`aws.ec2.Subnet.PrivateDnsNameOptionsOnLaunch.HostnameType.ip_name` のような
structural form を出すようになった。

一方、consumer 側には 5+ parts を古い形として読む経路が残っている。
`carina-core/src/utils.rs:88-114` の `NamespacedId::parse` 5+ parts branch
は、`provider.<service>.<resource>.TypeName.value` を前提に `TypeName` を
index 3 へ固定している。対象値をこの branch に入れると次のように割れる。

```text
parts    = [aws, ec2, Subnet, PrivateDnsNameOptionsOnLaunch, HostnameType, ip_name]
type     = PrivateDnsNameOptionsOnLaunch
value    = HostnameType.ip_name
wire-ish = HostnameType.ip-name
```

`convert_enum_value` は `carina-core/src/utils.rs:455-479` にあり、
docstring も 2/3/4/5-part の形だけを列挙している。実装は
`NamespacedId::parse(value).map_or(value, |id| id.value())` なので、
producer が 6 parts を出すようになった後も同じ位置決めを踏む。

同じファイルには schema-aware の値抽出も既に存在する。
`extract_enum_value_with_values` は
`carina-core/src/utils.rs:361-418` にあり、known valid values を使って
tail を照合する。dotted value のために単純な last-segment extraction を
避け、uppercase-led segment の後ろを candidate として valid values と
比較する作りである。carina#3383 の PR-A では core 内部の
`resolve_value_alias` もこの方向へ寄せられており、`carina-core/src/value.rs:1639-1657`
では schema から `valid_values` を集められる場合だけ
`extract_enum_value_with_values` を呼んでいる。

残っている drift は、provider wire-format site がまだ
`convert_enum_value` を使っている点である。producer は「構造を持った
identity」を出しているのに、consumer は「index 3 が type name」と読む。
この二つが同時に成立しない。

aws-types の重複についても確認した。`carina-provider-aws` 側の
`carina-aws-types` では、`carina-aws-types/src/lib.rs:130-202` の
`hand_written_string_enum_identities_are_structural` が hand-written enum の
structural identity を検査しており、S3 lifecycle も
`aws.s3.BucketLifecycleConfiguration.LifecycleRule.Status` のように中間
segment を持つ（同ファイル `:237-243`）。EC2 Subnet については
`carina-provider-aws/carina-provider-aws/src/schemas/generated/ec2/subnet.rs:114-116`
が `aws.ec2.Subnet.PrivateDnsNameOptionsOnLaunch` を namespace として
`HostnameType` を構築している。

`carina-provider-awscc` 側は、CLAUDE.md には local copy と書かれているが、
この checkout の `carina-provider-awscc/carina-provider-awscc/Cargo.toml:27`
は次の通りで、`carina-aws-types` を `carina-provider-aws` の git dependency
として参照している。

```toml
carina-aws-types = { git = "https://github.com/carina-rs/carina-provider-aws", rev = "cacba367d2b423fed0c56d11c3dbea4ad055c8e9" }
```

これは rollout の scope に効く。awscc sweep PR は `carina-aws-types` を直接
編集しない可能性が高く、`carina-provider-aws` 側で structural identity と
新しい public API surface を持つ rev へ `Cargo.toml` を上げることが、
awscc 側の aws-types 更新になる。memory 文書や repo 内の古い説明は、両
provider repo に duplicated local copy があると書いていることがあるが、その
snapshot は stale である。現在の topology は「`carina-provider-aws` が
`carina-aws-types` を owned し、両 provider が consume する」である。

awscc の generated schema でも
`carina-provider-awscc/carina-provider-awscc/src/schemas/generated/ec2/subnet.rs:148-150`
が同じ structural namespace を出し、generated docs も
`aws.ec2.Subnet.PrivateDnsNameOptionsOnLaunch.HostnameType.ip_name` を表示して
いる。現 checkout で確認できる範囲では、非 IAM enum の stale flat identity
は producer 側には見つからない。古い flat 形は
`carina-provider-aws/carina-provider-aws/src/tests.rs:637-670` の provider unit
test fixture に残っているだけで、これは consumer sweep 時に test 更新対象に
なる。

## なぜ schema-aware 方向が正しいか

CLAUDE.md Part 1 の root-cause lens で見ると、Subnet の site だけに
`strip_prefix("HostnameType.")` のような guard を入れるのは誤りである。
同じ producer drift は、nested enum を持つ次の provider wire-format site
でも再発しうる。症状が `ModifySubnetAttribute` に出ただけで、壊れている
不変条件は「enum identity を schema なしで位置決めできる」という前提である。

さらに long-term / type-safety lens では、`&str -> &str` の
`convert_enum_value` が残る限り、次の caller が schema を持っているにも
かかわらず schema-free fallback を選べてしまう。これは「新しい caller が
明日増えたら、同じ作法を覚えていなければならない」形で、broken state を
型で表現できてしまう。

schema-aware extraction はこの曖昧さを上流で消す。dotted display string
だけからは、`PrivateDnsNameOptionsOnLaunch` が struct segment なのか
type name なのかを一般には決められない。だが `TypeIdentity` か
`AttributeType` があれば、provider、structural segments、kind、valid
values のいずれかを使って「ここから先が enum value」と決められる。
producer が structural identity を出す設計に移った以上、consumer も同じ
schema を見て切るべきである。

consumer は dotted display string をパーツ数で読まない。schema を持つ
caller は schema を引き、持たない caller は存在しないのが目標である。

## API 形状の提案

推す案は、`convert_enum_value` を同じ名前のまま延命することではなく、
削除を基本にして、schema-aware API を一つだけ public surface として置く
形である。既存の `extract_enum_value_with_values` は
`carina-core/src/utils.rs:388` では `&[&str]` を受ける lower-level helper
だが、責務は新 API と重なる。実装 PR ではこれを public API として残さず、
`pub(crate)` の内部 helper に落とす。新 API の values 検証で同じ
case-insensitive tail 照合が必要になるため、役割を「public extractor」から
「enum wire extraction の検証部品」へ狭めるのがよい。

```rust
pub struct EnumWireValue(String);

impl EnumWireValue {
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}

pub fn extract_enum_wire_value(
    input: &str,
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
) -> Result<EnumWireValue, EnumWireValueError>;
```

この形を推す理由は二つある。第一に、`AttributeType` は
`shape_ref_free` / `Schema::shape_of` を通じて `Shape::Enum { identity,
values, dsl_aliases, .. }` を得られるため、caller が `TypeIdentity` と
`valid_values` を別々に手で揃える必要がない。第二に、validator-only enum
でも処理を分岐させなくてよい。`values` が `None` の場合も、`TypeIdentity`
の `provider + segments + kind` を display prefix として扱い、その prefix
を一致確認したうえでちょうどそこまでを strip する。`values` は parse の
必須材料ではなく、存在する場合に「切り出した値が列挙値に含まれる」ことを
検証する材料である。

`EnumWireValueError` は user-facing validation error ではない。想定する
variant は、identity prefix mismatch（`provider.segments.kind` が入力の先頭
segments と一致しない）、`values: Some(...)` があるのに切り出した値が含まれ
ない、入力が空または value segment を持たない、の三つで十分である。provider
site はこれを `?` で `ProviderError::invariant` 相当へ変換する。これは
「ユーザーの CRN が悪い」ではなく、「schema と state / provider conversion が
ずれているので bug として報告してほしい」という surface である。

`convert_enum_value(value, identity)` のような in-place 変更は避ける。
名前が schema-free の成功体験を残し、既存 call site の機械置換でまた
パーツ数ベースの考え方へ戻りやすい。実装 PR の既定動作は
`convert_enum_value` と `is_dsl_enum_format` の削除である。どうしても
schema-less 表示だけが残る場合は、別名の薄い helper を core 内部に閉じる。
その helper は provider wire-format site から import できない名前と visibility
にする。

現時点で本当に schema-less と言える production site は二つだけである。
`carina-core/src/value.rs:457-475` の `format_value_into` は `Value` だけを
受け取り、attribute name、resource type、schema を持たない再帰 formatter
である。1段上の caller も `carina-core/src/value.rs:345-355` の
`format_value` / `format_value_with_key` と、`carina-core/src/value.rs:1142-1145`
の `inline_width` であり、どちらも generic display / width 計算で schema を
持たない。

`carina-core/src/plan_tree.rs:289-305` の `extract_compact_hint` も
`ResourceRef` の attributes から表示ヒントを作るだけで、attribute の型情報を
受け取らない。1段上の caller は `carina-cli/src/display/mod.rs:55-63` の
`format_compact_name` と `carina-tui/src/app/mod.rs:645-656` の anonymous
resource display であり、どちらも resource display context は持つが schema
registry は持っていない。したがって削除 PR で schema を通せない場合に限り、
この二つの表示面だけが別名の core-internal helper を正当化する。

逆に `carina-core/src/value.rs:1639-1657` の
`resolve_value_alias` は resource type、attribute name、schema slice を持つ
ので、schema-less helper を正当化しない。そこは新 API に寄せ、schema が
見つからない場合は alias 解決を諦めるか、schema 不備として扱うべきである。

## Call-site の before / after

provider 側の call site は、raw string を直接 AWS spelling へ寄せる形から、
schema-aware extraction で `EnumWireValue` を得てから provider 固有変換を
掛ける形へ変える。Subnet の該当 site は
`carina-provider-aws/carina-provider-aws/src/services/ec2/subnet.rs:203` で、
before は次の通りである。

```rust
let hostname_val = convert_enum_value(ht).replace('_', "-");
```

after は概念的に次の形になる。`hostname_type_attr` の取り方は provider 側の
schema helper に寄せるが、`replace('_', "-")` は AWS provider に残す。

```rust
let hostname = extract_enum_wire_value(ht, hostname_type_attr, defs)?;
let hostname_val = aws_enum_hyphenated(&hostname);
```

awscc 側では `carina-provider-awscc/carina-provider-awscc/src/provider/conversion.rs:281-288`
が代表例である。現状は values 付き enum だけ
`extract_enum_value_with_values` を使い、validator-only enum では
`convert_enum_value` に戻る。

```rust
let resolved = if let Shape::Enum {
    values: Some(values),
    dsl_aliases,
    ..
} = shape
{
    let valid: Vec<&str> = values.iter().map(String::as_str).collect();
    let raw_extracted = extract_enum_value_with_values(s, &valid);
    carina_core::schema::DslMap::new(dsl_aliases, None).api_for(raw_extracted)
} else {
    convert_enum_value(s).replace('_', "-")
};
```

after は values の有無で extraction を分けない。`Shape::Enum` から identity、
values、alias table をまとめて渡し、values があれば検証、なければ identity
prefix strip だけを行う。

```rust
let wire = extract_enum_wire_value(s, attr_type, defs)?;
let resolved = match shape {
    Shape::Enum { dsl_aliases, .. } => {
        carina_core::schema::DslMap::new(dsl_aliases, None).api_for(wire.as_str())
    }
    _ => unreachable!("is_namespaced_enum gate already checked Shape::Enum"),
};
```

## Call-site インベントリ

2026-06-10 に次の3リポジトリで新規に実行した。

```bash
rg -n "convert_enum_value" --type rust .
rg -n "convert_enum_value" --type rust /Users/mizzy/src/github.com/carina-rs/carina-provider-aws
rg -n "convert_enum_value" --type rust /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc
```

再現しやすいように、表は `rg` が返した raw hit 行数と、実装で置換すべき
unique production call site を分ける。raw hit には import、doc comment、
test、関数定義も含まれる。

| repo | rg total hits | unique production call sites | test/doc/import/definition/comment hits |
| --- | ---: | ---: | ---: |
| carina | 46 | 3 | 43 |
| carina-provider-aws | 19 | 5 | 14 |
| carina-provider-awscc | 2 | 1 | 1 |

この inventory の目的は、schema を持つ consumer と持たない表示専用 site を
分けることである。carina core の production call site は三つで、
`carina-core/src/value.rs:470-471` と
`carina-core/src/plan_tree.rs:303-305` が schema-less 表示短縮、
`carina-core/src/value.rs:1639-1657` が schema を持てる alias fallback である。
前二者だけが、もし削除 PR で schema を通せないと証明された場合に、
別名の display-only helper を正当化する。

provider-aws の production call site は五つである。Subnet の
`carina-provider-aws/carina-provider-aws/src/services/ec2/subnet.rs:85` と
`:203` は AWS の `_` → `-` 変換前に値を取り出す。security group の
`carina-provider-aws/carina-provider-aws/src/ec2_security_group_rules.rs:367-373`
は protocol の `all` → `-1` 特例前に使う。S3 の `services/s3/bucket.rs` と
`services/s3/bucket_acl.rs` は S3 enum/string 入力を provider value に寄せる。
これらは wire-format site なので、display-only helper ではなく schema-aware
API に接続する。

provider-awscc の production call site は
`carina-provider-awscc/carina-provider-awscc/src/provider/conversion.rs:281-288`
の一つである。ここは values を持つ shape では既に
`extract_enum_value_with_values` を使うが、validator-only enum fallback で
`convert_enum_value(s).replace('_', "-")` に戻る。新 API はこの branch を
消すためのものでもある。

## ロールアウト計画

最初の PR はこの design PR で、consumer-side cleanup を #3409 の
state ↔ DSL 双方向正規化とは別スコープとして固定する。carina 本体では
通常 CI の `cargo check --all-targets --all-features`、clippy、
`cargo nextest run --workspace --all-features`、doctest、provider boundary
系の `scripts/check-*.sh` が gate になる。local green は CI green ではない
ので、PR を開く前に該当する `scripts/check-*.sh` をローカルでも走らせる。

次に carina-core 実装 PR を出す。ここでは `EnumWireValue` と
schema-aware extractor を追加し、`convert_enum_value` は削除を既定にする。
core 内の schema を持てる call site は新 API へ寄せる。schema-less 表示 site
が本当に残る場合だけ、名前を変えた core-internal helper を追加する。回帰
テストは、6-segment structural identity と dotted value の両方を含める。
`carina-core` crate scoped の check/test を先に通し、最後に workspace gate
と `scripts/check-*.sh` をローカルで通す。

その次が carina-provider-aws sweep PR である。Subnet、security group、
S3 の wire-format site を schema-aware API に移す。AWS SDK spelling の
`replace('_', "-")` や protocol の `all` → `-1` は provider 側の責務として
残す。CI gate は `cargo check`、`cargo test`、clippy、`check-carina-pin`、
`check-string-enum-aliases`、Codegen Check である。PR を開く前に
`scripts/check-carina-pin.sh` と `scripts/check-string-enum-aliases.sh` を
ローカルでも走らせる。実 AWS に触る acceptance test は明示指示があるとき
だけ走らせるが、この issue の確認点として
`acceptance-tests/tag_deletion/ec2_subnet` step2 を PR description に明記する。

最後に carina-provider-awscc sweep PR を出す。`provider/conversion.rs` の
validator-only fallback を schema-aware extractor へ寄せ、values を持つ
shape で既に使っている values-aware extraction と同じ考え方に揃える。
この checkout では awscc が `carina-aws-types` を git dependency として
consume しているため、awscc sweep PR は `carina-aws-types` を直接編集しない
可能性がある。必要なのは、carina-provider-aws 側の rev を structural
identity / new public API を含む commit へ上げ、awscc 側の conversion site を
新 API へ寄せることである。CI gate は `cargo check`、`cargo test`、clippy、
`check-carina-pin`、Codegen Check、Check Docs Drift である。PR を開く前に
`scripts/check-carina-pin.sh` と `scripts/check-docs-drift.sh` をローカルでも
走らせる。

## 型安全レンズ

`EnumWireValue` は、schema-aware extractor だけが作れる newtype とする。
constructor は crate 内に閉じ、外からは `as_str` と `into_string` だけを
公開する。

```rust
pub struct EnumWireValue(String);

impl EnumWireValue {
    pub(crate) fn new(value: String) -> Self;
    pub fn as_str(&self) -> &str;
}
```

`pub(crate)` constructor なので、provider crate は `EnumWireValue` を直接
作れない。public に値を得る唯一の経路は carina-core の schema-aware
extractor であり、これが typestate の enforcement になる。provider unit test
が helper に wire value を渡したい場合も、test-only constructor は provider
側に足さない。小さな `TypeIdentity` / `AttributeType` fixture を組み、実際の
extractor を通して `EnumWireValue` を作る。

provider 側には、raw `&str` を受ける helper と enum wire value を受ける
helper を分けて置く。たとえば AWS の hyphen 変換は次のように、enum path
では `EnumWireValue` を要求する。

```rust
fn aws_enum_hyphenated(value: &EnumWireValue) -> String {
    value.as_str().replace('_', "-")
}
```

この形にすると、structural identity を含む raw `&str` をそのまま
AWS enum helper に渡す code は compile-fail になる。schema-aware extractor
を先に通した code だけが `EnumWireValue` を得られるので、broken shape を
provider wire-format site へ持ち込めない。

raw `&str` は引き続き non-enum text path に残す。bucket name、tag value、
plain string attribute まで newtype 化する必要はない。型で分けるべきなのは
「schema で enum value と判定済みの wire candidate」と「ただの文字列」で
あり、ここを同じ `&str` にしていることが今回の再発点である。

## スコープ外 / 非ゴール

この文書は #3409 の bidirectional-normalization contract を吸収しない。
state に保存された API spelling を DSL spelling へ lift する話、
`StringEnum` と `CustomEnum` の統合、state ↔ DSL の差分ゼロ保証は
`2026-06-07-enum-state-coherence-design.md` の範囲で扱う。

また、AWS 固有の `_` ↔ `-` 変換は provider 側に置いたままにする。
carina-core が知るべきなのは schema に基づく enum value extraction までで、
AWS SDK spelling、protocol の `all` → `-1`、サービス固有の JSON 形は
provider の wire-format 層が責任を持つ。

この design PR では実装もテスト追加も行わない。目的は drift の位置と
cross-repo rollout の順序を固定し、implementation PR が consumer site ごとの
小さな guard に流れないようにすることである。

## 残課題 / ユーザに判断仰ぐ点

API 名は本文では `extract_enum_wire_value` を仮名にしたが、
`enum_wire_value_for_attr` のように戻り値と schema 入力を前面に出す名前も
ありうる。newtype 名も `EnumWireValue`、`ResolvedEnumValue`、
`ProviderEnumValue` のどれが provider author に誤読されにくいかを決めたい。

error 型名も `EnumWireValueError` を仮名にした。variant の意味は本文で固定
したので、残る判断は型名を `EnumWireValueError` にするか、
`EnumExtractionError` のように動作名へ寄せるかだけである。
