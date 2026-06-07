# クロス名前空間 identity 型の正規化 (carina#3413)

<!-- derived-from ./2026-06-07-enum-state-coherence-design.md -->

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。
本 issue (carina#3413) の Acceptance のうち「documented contract for
which identity-shaped types the project considers cross-namespace
compatible, and the mechanism that enforces it」を本 PR で満たす。
enforcement と回帰テストは後続実装 PR（carina-provider-aws /
carina-provider-awscc / carina）で扱う。

## 課題 (carina#3413)

AWS の同一概念を指す identity 値が、`aws.*` provider と `awscc.*`
provider のそれぞれで別の型として宣言されている。carina#3412 で
`AttrTypeKind::Enum` が `TypeIdentity` ベースの厳密な
`is_assignable_to` を持つようになった結果、同じ AWS 値であっても
`provider` セグメントが異なる identity は `validate` 段階で代入を拒
否される。

`carina-rs/infra` の `aws/management/identity-center/main.crn` での再現:

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

12 桁の AWS アカウント ID は `sts:GetCallerIdentity` が返す AWS 全体
で唯一の値で、`aws.AccountId` と `awscc.AccountId` の間に意味の差は
ない。同じ問題が IAM role/policy/OIDC provider の Arn など、AWS が
region-less / account-wide / global に扱う identity 全般で起きる:

- `aws.iam.Role.Arn` ↔ `awscc.iam.Role.Arn`
- `aws.iam.Policy.Arn` ↔ `awscc.iam.Policy.Arn`
- `aws.iam.OidcProvider.Arn` ↔ `awscc.iam.OidcProvider.Arn`
- `aws.AccountId` ↔ `awscc.AccountId`

issue 本文は三つの取りうる挙動を整理している。

1. **単一 canonical newtype**: identity 型を provider 中立な単一の
   `TypeIdentity` に統合し、両 provider のリソース属性がその型を返
   す。validate には何も足さなくても、`==` で一致するため代入が通る。
2. **構造的互換アーム**: 異なる名前空間の同種 identity 型を
   `is_assignable_to` が双方向に許容する。carina#3412 が Enum に追
   加した `TypeIdentity::assignable_to` を一般化する形。
3. **provider 側 alias 宣言**: 各 provider が自分の identity newtype
   に「もう片方の名前空間の等価物」を alias として宣言し、validator
   が alias を尊重する。

本設計は **(1) を採り、carina-aws-types を単一の出どころとして両
provider に共有させる** 道を取る。

## 採用する contract: (1) 単一 canonical newtype + types crate 共有

AWS が global に1つの値として扱う identity（AccountId, IAM の Arn 系,
将来追加されうる Route53 HostedZoneId 等）は、`aws.*` 名前空間の単一
の `TypeIdentity` で表す。`awscc.*` 名前空間にこれらの identity の写
しは作らない。

実装の置き場所は **`carina-provider-aws` リポジトリのサブ crate
`carina-aws-types`** に集約する。今 `carina-provider-aws` と
`carina-provider-awscc` の双方に重複して存在しているこの crate を、
`carina-provider-aws` 側のものを単一の真実の出どころとし、
`carina-provider-awscc` から git dependency で参照する。

これに合わせて、現在 `carina-provider-awscc` 内にある同名の
`carina-aws-types` は `carina-awscc-types` に rename し、awscc 固有
の identity 専用に役割を絞る。

### 他案を採らない理由

(2) と (3) は型レベルで broken state を表現可能なまま残す。

(2) は `is_assignable_to` の Enum→Enum arm に互換ルールを加えるだけ
の最小侵襲案だが、`TypeIdentity` 同士の `==` は依然 false のままに
なる。validate を通る経路では救われるが、`TypeIdentity` を `==` で
比較する別経路（state diff、canonicalize、シリアライズ、ハッシュキ
ー化）を将来の caller が作ったとき、同じバグが再生産される。これは
CLAUDE.md が繰り返し戒めている「runtime patch at multiple consumer
sites disguised as a root-cause fix」のパターン。

(3) も同じ難点を持つ。さらに alias 表が両 provider に分散するため、
真実の出どころが複数になる。aws 側で identity が増えたとき awscc 側
の alias 表に追加し忘れると、issue#3413 と同じバグが静かに再発する。
過去メモリ `project_carina3385_iam_structural_identity_awscc_gap` が
記録しているように、awscc 側の types crate コピーが aws 側の更新に
追従し損ねて bug が起きた前例がそのまま当てはまる。

(1) は両 provider が物理的に同じ Rust 型を使うので、発行される
`TypeIdentity` が同一であることが型レベルで保証される。新しい
identity が将来 `carina-aws-types` に追加された時、awscc 側で覚えて
おく作業は要らず、登録漏れによる再発が構造的に発生しない。

## canonical 化の線引き

すべての identity を `aws.*` に寄せるわけではない。`carina-rs/carina`
プロジェクトは aws SDK ベースの `carina-provider-aws` と CloudControl
ベースの `carina-provider-awscc` を独立した provider として意図的に
共存させており（メモリ `project_dual_provider_intentional`）、リソ
ースを「どちらの provider で管理しているか」という provenance（出所）
の意味を持つ型まで統合すると、state ownership の境界が曖昧になる。

canonical 化する identity は次の条件を満たすものに限る。

- AWS が region-less / account-wide / global に 1 つの値として扱う。
  複数のリージョン・複数のアカウントから見ても同じ値を指す。
- そのリソースを「どちらの provider で管理しているか」が値の意味に
  影響しない。つまり、ある IAM role を `aws.iam.Role` で管理してい
  ようと `awscc.iam.Role` で管理していようと、その Arn が指す対象は
  AWS 上で同一であり、第三のリソース（例: usecase argument, SSO
  assignment）から見たときに区別する理由がない。

この線引きから、初期 canonical 化対象は次の 4 種に限定する:

- `AccountId`
- `iam.Role.Arn`
- `iam.Policy.Arn`
- `iam.OidcProvider.Arn`

逆に canonical 化しないもの:

- リージョン / アカウントスコープを持つ通常のリソース Arn
  （`s3.Bucket.Arn`, `ec2.Instance.Arn`, `lambda.Function.Arn` など）。
  これらは provenance の意味を持つ。`aws.s3.Bucket` で作ったバケッ
  トを `awscc.s3.Bucket` のリソース引数に渡せると state の所有権が
  曖昧になる。現状の validate が拒否する挙動が正しい。
- provider 固有の表現を持つ enum や設定値。CloudControl と SDK で
  文字列形が異なる場合がある。
- リソース型そのもの（`aws.iam.Role` と `awscc.iam.Role` のように、
  リソースとして宣言される側）。これは provenance を持つので別物の
  まま。canonical 化するのは identity 値（戻り値・参照値）だけ。

初期対象を 4 種に限定するのは、issue#3413 の acceptance を満たすた
めの最小集合がここで、かつ各 identity の AWS 上のスコープ（global
service / region-less / account-wide）の判定を 1 件ずつ確認しながら
進める方が、自動マッチで誤った範囲まで取り込むより安全だから。後で
追加候補が出たとき（Route53 HostedZoneId, Organizations の各種 ID,
CloudFront の各種 ID 等）は同じ線引きで個別に判断し、follow-up issue
で広げる。

## 重複解消の構造

現状、`carina-aws-types` は `carina-provider-aws` と
`carina-provider-awscc` の双方に独立した複製として存在している
（メモリ `project_aws_types_triplicated_copies`。本リポジトリ
`carina-rs/carina` のコピーは commit `148553e0` で削除済みで、現存
するのは provider 側 2 コピー）。本設計では:

- `carina-provider-aws/carina-aws-types` を単一の真実の出どころとす
  る。AWS 全体で global / region-less / account-wide な identity 型
  はここに置く。
- `carina-provider-awscc` は `carina-aws-types` を git dependency で
  参照する。awscc codegen が identity 型を発行する箇所で、aws 側に対
  応概念がある identity については `carina-aws-types` の型を使う。
- `carina-provider-awscc` 内にある現 `carina-aws-types` は
  `carina-awscc-types` に rename し、awscc 固有の identity 専用にす
  る。元々ここにあった型は基本そのまま新名称の crate に残す。本設計
  で canonical 化対象とする 4 種の identity（後述）の awscc 側コピー
  だけは削除し、`carina-aws-types` の型を参照する形に置き換える。

`carina-aws-types` を独立リポジトリに切り出す案も検討したが採らない。
`carina-provider-aws` がこの crate の主要な利用者であり、リリース粒
度が同期しているほうが運用が軽い。aws / awscc どちらの provider が
identity を増やしたときも、`carina-provider-aws` リポジトリ内で
types crate と provider が同時にリリースされる。awscc は git
dependency を rev-bump で更新する。`carina-provider-awscc` が
`carina-provider-aws` の crate を参照するという依存方向は概念上の
非対称を生むが、`carina-aws-types` の役割を「AWS が定義する
identity 値の型表現」に絞っているかぎり、aws-provider 固有のロジッ
クは含まれないため実害は薄い。将来必要が出たら git subtree split で
独立リポジトリに切り出すことはいつでもできる。

## identity 発行ルール

`carina-provider-awscc` の codegen が CloudControl schema を carina
schema に変換する過程で、属性の型が canonical 化対象の identity に
当たる場合に `carina-aws-types` の型 (=`aws.*` の `TypeIdentity`) を
発行する。それ以外は従来通り `carina-awscc-types`（旧 awscc 側
`carina-aws-types`）の型を発行する。

判定方式は明示 override 表を採る。命名規約ベースのマッチ（"...Arn" /
"AccountId" / 既知パターンによる自動推定）は採らない。誤マッチが後
で発見しづらく、結果としてサイレントに型を間違えるリスクが大きいた
め。

明示 override 表の初期エントリは canonical 化対象の 4 種に対応する
CloudControl 側の type 表現を列挙したものになる。具体的な表のシェ
イプ（YAML / Rust の const / codegen 内の HashMap 等）は実装 PR で
決める。設計レベルで重要なのは、

- 真実の出どころは `carina-aws-types` 1 箇所
- awscc codegen は「この CloudControl type に出会ったら、
  `carina-aws-types` のこの型を発行する」というマッピングを持つ
- マッピングは明示宣言のみ、命名規約マッチは使わない

この 3 点。

## carina-core への影響

なし。`is_assignable_to` には触らない。`TypeIdentity` の比較規則も
触らない。型システムは provider が発行する `TypeIdentity` をそのま
ま信じる。両 provider が同じ `TypeIdentity` を発行するようになれば
`==` で一致し、cross-namespace 代入は自然に通る。

これが (2) や (3) と比べた (1) の最大の利点で、carina-core 側に新
しい runtime check を入れる必要がない。CLAUDE.md の「broken state
を表現不可能にする」原則を、provider 側の codegen レベルで満たす。

## 回帰テストの置き場

`carina-core` に mock provider ベースの test fixture を追加する。
mock provider に同一の `TypeIdentity`（例: provider セグメント `aws`、
segments `[]`、kind `AccountId`）を発行させた 2 つの「異なる名前空
間に見える」mock resource を用意し、片方の戻り値を他方の入力に渡す
DSL が validate を通過することを assert する。

これは provider 横断の動作確認だが、carina-core 単体で完結する。
実際の aws / awscc plugin を読む必要はない。`is_assignable_to` の
規則が「同一 `TypeIdentity` を発行する任意の 2 つの provider に対し
て対称的に動く」ことが test の主張で、それは mock で十分検証できる。

実 provider の動作確認は `carina-rs/infra` の
`aws/management/identity-center/main.crn` を `carina validate` で実
行することを実装 PR の acceptance に含める。

## 段階リリース

設計 PR が本 PR。実装は次の 3 つの provider repo / carina repo に
分かれる:

1. carina（本リポジトリ）: 上記回帰テストの追加。design doc PR（本
   PR）のマージ後、別 PR で対応する。
2. carina-provider-aws: `carina-aws-types` に canonical identity 型
   の整備。元々 aws 側に存在する型なので、構造の見直しと公開 API の
   整理が主。
3. carina-provider-awscc:
   - `carina-aws-types` を git dependency に切り替え
   - 旧 `carina-aws-types` を `carina-awscc-types` に rename
   - codegen に override 表を入れ、canonical 化対象の identity を
     `carina-aws-types` の型で発行
   - dep-bump で carina-provider-aws の rev を取り込む

回帰テスト (1) は mock provider で完結するため (2)(3) と並行に進め
られる。実 provider 側の作業は (2) → (3) の順序になる。aws 側の
types crate が確定してから awscc 側がそれを参照し、dep-bump で取り
込む。最終的な acceptance 確認として `carina-rs/infra` の
`aws/management/identity-center/main.crn` を `carina validate` で実
行する手順を (3) の PR に含める。

初期 override 表は canonical 化対象 4 種で開始し、それ以外の
identity 候補（Route53 HostedZoneId, Organizations の各種 ID,
CloudFront の各種 ID, S3 bucket policy ARN など）は同じ線引きで個別
に判断し、必要が生じた時点で follow-up issue として追加する。本設計
では初期 4 種に絞り込むことで、各 identity の AWS スコープ判定を 1
件ずつ確認しながら進める。

## 過去メモリとの繋がり

本設計が念頭に置く過去の文脈:

- `project_dual_provider_intentional`: aws と awscc を独立 provider
  として共存させるという意思決定。本設計の「リソース型そのものは
  canonical 化しない、identity 値だけ」という線引きの根拠。
- `project_aws_types_triplicated_copies` / `project_carina3385_iam_structural_identity_awscc_gap`:
  `carina-aws-types` が provider 2 リポジトリに独立コピーとして存在
  し、片方の更新がもう片方に追従し損ねて bug が再生産された前例。本
  設計の types crate 一元化はこの重複を構造的に解消する。
- `feedback_provider_boundary_no_dedup`: 「aws/awscc を超えた重複排
  除はするな」というルール。本設計はリソース型・provider ロジック
  の重複排除を求めるものではない。共有するのは identity 値の型表現
  だけで、これは aws/awscc どちらの SDK にも依存しない純粋型データ
  であるため、本ルールに抵触しない。
- carina#3412 の Enum unification: 本 issue は #3412 が
  `is_assignable_to` を `TypeIdentity` ベースで厳密化した結果として
  顕在化した。#3412 自体は正しい（カナリアとして役立つ）改善で、
  本設計は #3412 を巻き戻すのではなく、provider 側で identity を一
  致させることで type-safe に解消する道を取る。
