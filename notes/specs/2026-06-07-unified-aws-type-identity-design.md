# AWS 型 identity の統一 (carina#3413)

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。
本 issue (carina#3413) の Acceptance のうち documented contract を本
PR で満たす。enforcement と回帰テストは後続実装 PR で扱う。

本ドキュメントは 2026-06-07 に carina#3418 として merge されたが
carina#3420 で revert された design doc の置き換え。前 doc は AWS が
global / region-less / account-wide に扱う identity (`AccountId` と
IAM の三種の Arn) だけを canonical 化する矮小スコープを採っていた。
本 doc はその境界を捨て、AWS 上で同じ値を指す identity すべてを統一
する設計に書き直す。

## 課題

`carina-rs/infra` の `aws/management/identity-center/main.crn` で:

```crn
let caller = read aws.sts.CallerIdentity {}

awscc.sso.Assignment {
  ...
  target_id   = caller.account_id      # aws.AccountId
  target_type = awscc.sso.Assignment.TargetType.aws_account
}
```

```
Error: awscc.sso.Assignment.: cannot assign aws.AccountId to 'target_id':
       expected awscc.AccountId, got aws.AccountId (from caller.account_id)
```

issue#3413 が指す当面の症状はこれだが、根は深い。carina#3412 で
`AttrTypeKind::Enum` が `TypeIdentity` ベースの厳密な
`is_assignable_to` を持つようになった結果、provider 軸の違いを単純に
拒否するようになった。そして `carina-provider-aws` と
`carina-provider-awscc` の双方が `carina-aws-types` という同名 crate
を独立した複製として持っており、片方は `PROVIDER_NAME = "aws"`、もう
片方は `PROVIDER_NAME = "awscc"` を発行する。同じ AWS 値を表すための
identity 型が、provider 軸違いで二重に存在している状態。

具体的に何が二重化しているか確認する。awscc 側 crate
(`carina-provider-awscc/carina-aws-types`) の `pub fn` のうち aws 側
crate (`carina-provider-aws/carina-aws-types`) に存在しないものは:

- `sso_instance_arn` / `sso_permission_set_arn` / `sso_principal_id`
- `identity_store_id` / `validate_identity_store_id`
- `kms_key_id` (constructor のみ。validator は無い)
- `region_dsl_aliases`

これらは「awscc 固有」ではなく、単に aws 側にリソース定義が無いため
constructor が無いだけ。AWS SSO / Identity Store / KMS は aws SDK で
も標準的に扱える概念。

残りの大量の関数 (`vpc_id`, `subnet_id`, `security_group_id`,
`internet_gateway_id`, `route_table_id`, ..., `validate_arn`,
`find_matching_enum_value`, `canonicalize_enum_value`,
`validate_iam_arn`, `is_valid_region`, `validate_tags_map`,
`provider_type`, `provider_bare_type`, `iam_policy_document`, ...) は
aws/awscc の双方に存在し、`PROVIDER_NAME` 以外は実装も同じ。

つまり awscc 側 crate のほぼ全関数は aws 側 crate の重複コピー。
過去メモリ `project_aws_types_triplicated_copies` /
`project_carina3385_iam_structural_identity_awscc_gap` が記録してい
る通り、片方の更新がもう片方に追従し損ねる事故は実際に何度も起きた。

## 採用する contract

AWS が同じ値として扱う identity 型は、Carina 側でも単一の
`TypeIdentity` で表す。aws/awscc どちらの provider 経由で取得・参照
しても、AWS 上の同じ実体を指すなら同じ `TypeIdentity` を発行する。

具体的には:

- `TypeIdentity` の `provider` 軸として `awscc` を発行する道を一切閉
  じる。AWS のリソースを指す identity の `provider` は `Some("aws")`
  のみ。これは `vpc_id` のような region-scope identity も `region`
  のような global identity も区別しない。
- `carina-aws-types` (carina-provider-aws のサブクレート) を AWS が
  定義する identity 値の単一の置き場とする。
- `carina-provider-awscc` 側の `carina-aws-types` crate は廃止する。
  awscc 側で必要な identity / 純粋関数 / helper は upstream
  `carina-aws-types` を git dependency として参照する。
- 現状 awscc 側にしかない constructor (SSO, Identity Store, KMS の
  helper) は upstream `carina-aws-types` に追加し、awscc 側からはそ
  ちらを参照する形に切り替える。

### なぜこうするか

`aws.s3.Bucket` で作ったバケットと `awscc.s3.Bucket` で作ったバケッ
トが Carina の state 上で別レコードになるのは事実だが、両者が指す
AWS 上の Arn は同じ値で、AWS の API も区別しない。「どちらの provider
で管理しているか」(provenance) は Carina 側の state 管理レイヤーの関
心事であって、型システムに持ち込むと AWS の現実と乖離する。

carina-core の `is_assignable_to` を緩める案 (前 design doc で
"option (2)" として却下した形) は、`TypeIdentity` の `==` が依然
false のままにすることになる。validate 経路は救えても、`==` を踏む
別経路 (state diff, canonicalize, serialize, hash key 化) で同じバ
グが将来の caller によって再生産される。これは CLAUDE.md の
「runtime patch at multiple consumer sites disguised as a root-cause
fix」のパターン。

両 provider が物理的に同じ Rust 型を呼ぶ形にすると、発行される
`TypeIdentity` が同一であることが型レベルで保証される。新しい
identity が将来 `carina-aws-types` に追加された時、awscc 側で何かを
覚えておく作業は不要で、登録漏れによる再発が構造的に発生しない。

### 矮小化案を採らない理由

前 design doc (carina#3418, revert 済) は「AWS が global / region-less
/ account-wide に扱う identity (4 種) だけを canonical 化、region
scope は別物のまま」という境界を引いた。これは間違いだった。

- region scope の identity (VPC ID 等) も AWS 上の値としては aws/awscc
  どちらの API で取得しても同じ文字列。型を別物にする AWS-grounded
  な理由がない。
- provenance を型で表現するという発想は、Carina 側の state 管理と
  AWS 側の値同一性を混同していた。
- 結果として「初期 4 種だけ」という線引きは恣意的で、同じ class の
  bug を sibling site に残した。CLAUDE.md の「never minimal fix in
  this PR, follow-up for the rest」に直接抵触。

「対象は AWS の全 identity 型」と境界を引き直す。

## 実装スコープ

実装 PR は 4 段階に分ける。

### PR 1: 本 design doc (carina)

本 PR。document only。

### PR 2: aws-types に必要な追加を全部入れる (carina-provider-aws)

awscc 側にしかない constructor を `carina-aws-types` に追加し、両
provider から使える形にする:

- `sso_instance_arn`, `sso_permission_set_arn`, `sso_principal_id`
  + 対応する `validate_*` (awscc 側から移植)
- `identity_store_id` + `validate_identity_store_id`
- `kms_key_id` (constructor + validator)
- `region_dsl_aliases`

いずれも `provider_type(...)` / `provider_bare_type(...)` を経由して
`PROVIDER_NAME = "aws"` を埋め込んだ TypeIdentity を発行する。

ここで追加されたものを使う caller (awscc 側) は PR 3 まで存在しない
ので、本 PR は純粋な追加で破壊変更を含まない。

### PR 3: awscc-types の解体 (carina-provider-awscc)

awscc 側の `carina-aws-types` crate を廃止する:

- `carina-provider-awscc/Cargo.toml` の path dep を git dep に切り替
  え (`carina-aws-types = { git = "https://github.com/carina-rs/carina-provider-aws", rev = "<PR 2 の merge commit>" }`)。
- awscc 側ローカルの `carina-aws-types/` ディレクトリを workspace
  members から外し、ディレクトリごと削除。
- 既存の `use carina_aws_types::*` (awscc 内のもの) はそのままで動
  くが、解決先が path dep から git dep に変わる。
- codegen が生成する `super::*` 形の参照、または直接 `carina_aws_types::*`
  形の参照を整理。`PROVIDER_NAME = "awscc"` を発行していた helper は
  awscc 側に残らないので、コード上の参照を `carina_aws_types::*` に統
  一する。
- generated コードを再生成し、`grep -rE "awscc\.<segment>" carina-provider-awscc/src/schemas/generated/` で
  `awscc.*` の TypeIdentity が一切出ないことを確認。

機械的な置換が大半で、人間が手で書く部分は限定的になるが、diff は
大きい。

### PR 4: mock-provider 回帰テスト (carina)

`carina-core/src/schema/tests.rs` (もしくは tests ディレクトリ) に
mock provider を 2 種類用意して、複数の identity (`vpc_id`,
`aws_account_id`, `iam_role_arn` などを代表として) について、両 provider
が同一 `TypeIdentity` を発行した場合に cross-provider 代入が
`is_assignable_to` を通過することを assert する。

negative 側として、`provider` 軸が違う TypeIdentity (例えば架空の
`gcp.AccountId`) との代入が依然拒否されることも 1 件は確認する。これ
は `assignable_rejects_same_kind_across_providers` がすでにカバーし
ているが、本 design doc が `provider` 軸そのものは廃止していない
(`provider = Some("aws")` 一択にするだけで `Option<String>` 型は残
る) ことを補強する意味で再掲する。

実 provider の動作確認として、`carina-rs/infra` の
`aws/management/identity-center/main.crn` を `carina validate` で実
行する手順を本 PR の acceptance に含める。

## carina-core への影響

なし。`is_assignable_to` には触らない。`TypeIdentity` の比較規則も
触らない。`TypeIdentity::new` の signature も触らない。

両 provider が物理的に同じ Rust 型を経由するようになれば、発行され
る `TypeIdentity` は `==` で一致し、cross-namespace 代入は自然に通る。
これが本設計の最大の利点で、carina-core 側に新しい runtime check を
入れる必要がない。

## scope の境界

含めない:

- DSL レベルの `aws.*` / `awscc.*` 名前空間自体は両方残る。`provider aws { ... }` /
  `provider awscc { ... }` の宣言や `aws.iam.Role` / `awscc.iam.Role`
  のような resource 型は両 provider が並立した形で書ける。本 design
  doc が統一するのは identity 値 (戻り値・参照値) の `TypeIdentity`
  だけで、リソース型そのものは別。
- `carina-aws-types` の物理配置 (carina-provider-aws のサブクレート
  のまま) は変えない。独立 repo / carina-core 側への移動は別問題。
- crate 名 (`carina-aws-types`) も変えない。中身は AWS の identity
  型置き場で、名前は内容を正確に示している。aws/awscc 両者が依存す
  る関係性は依存側の話。

含める:

- AWS 上で同じ値を指すすべての identity (region scope / account scope
  / global scope を問わず)。`vpc_id` / `subnet_id` / `security_group_id`
  / `region` / `account_id` / IAM の Arn 群 / SSO / KMS / その他。
- 現状 awscc 側にしかない constructor も aws-types に移し、awscc は
  そこを参照する形に統一。

## 過去メモリとの繋がり

本設計が念頭に置く過去の文脈:

- `project_aws_types_triplicated_copies`: `carina-aws-types` が複数
  リポジトリに重複コピーとして存在し、片方の更新がもう片方に追従し
  損ねて bug が出た前例。本設計はこの crate を 1 つに統合してその
  class の bug を構造的に解消する。
- `project_carina3385_iam_structural_identity_awscc_gap`: IAM の構
  造的 identity 修正が awscc 側の重複コピーに伝播せず再発した事例。
  同じ pattern が `is_assignable_to` 厳密化後に identity 全般で再発
  したのが issue#3413 の本質。
- `feedback_provider_boundary_no_dedup`: 「aws/awscc を超えて normalizer
  / handler / converter を抽象化するな」というルール。本設計はこのル
  ールに抵触しない。共有するのは identity 値の型表現 (`AttributeType`)
  だけで、aws/awscc どちらの SDK にも依存しない純粋な型データ。provider
  trait や normalizer は引き続き別実装。
- `project_dual_provider_intentional`: aws と awscc を独立 provider
  として共存させる意思決定。本設計は provider そのものの共存を妨げ
  ない。共有するのは identity 型だけ。
- carina#3412 の Enum unification: 本 issue は #3412 が
  `is_assignable_to` を `TypeIdentity` ベースで厳密化した結果として
  顕在化した。#3412 自体は正しい改善で、本設計は #3412 を巻き戻すの
  ではなく、provider 側で identity を一致させることで type-safe に
  解消する。
- 前 design doc carina#3418 (revert 済): AWS の global identity 4
  種だけに canonical 化を絞った矮小スコープ。CLAUDE.md の「never
  minimal fix in this PR, follow-up for the rest」に抵触する形にな
  っていたため revert された。本設計はその境界を捨て、AWS 全 identity
  に拡大する。
