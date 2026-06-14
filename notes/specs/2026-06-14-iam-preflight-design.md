# Apply role の IAM 権限事前チェック (carina#3496)

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。

本 issue (carina#3496) の Acceptance のうち、設計上の判断
（配置・解決方式・スコープ・厳格度・provider 境界の形）を本 PR で固
定する。実装は後続 PR チェーンで扱う。

## 課題

`carina apply` が CloudControl 経由でリソースを作成するとき、apply
role の IAM ポリシーに CloudControl handler が必要とする action が
欠けていると、その失敗は apply 実行中の 403 として初めて表に出る。
plan 時点では検知されない。partial create が走った後、依存待ちで
ハングし、cancel になり、orphan リソースの後始末まで発生する。

`carina-rs/infra` の Publish API ALB stack (`envs/registry/dev/publish`)
の例:

```
✗ Create awscc.elasticloadbalancingv2.TargetGroup ...
  → User: ... is not authorized to perform: ec2:DescribeInternetGateways
✗ Create awscc.elasticloadbalancingv2.LoadBalancer ...
  → User: ... is not authorized to perform: iam:CreateServiceLinkedRole
```

どちらも CloudFormation registry schema の create handler の
`permissions` に書かれており、apply の前に知り得た。

事前に検知できる材料は二つある:

1. **CFN registry schema の `handlers` block** — 各 registry resource type
   の JSON (例: `https://schema.cloudformation.us-east-1.amazonaws.com/aws-elasticloadbalancingv2-loadbalancer.json`)
   が、create / read / update / delete / list の各 lifecycle hook で
   必要な IAM action を列挙している。
2. **apply role の IAM identity policy** — STS `GetCallerIdentity` で
   特定し、IAM `GetRolePolicy` / `ListAttachedRolePolicies` で実体を
   取るか、`iam:SimulatePrincipalPolicy` で IAM に判定させるか。
   carina の awscc provider は CloudControl の `RoleArn` パラメータ
   を使っておらず、handler は caller の権限で動く。actor 決定は
   STS 一回で済む（provider 設定の `assume_role` がある場合も
   GetCallerIdentity が assumed role 側の ARN を返すので同じ経路）。

plan が提示する `+ create` / `~ update` / `- destroy` の各 effect に
対し、provider が「この op にはこの action 群が必要」と申告できれば、
host 側でそれを apply role の権限と突き合わせて plan 時に警告できる。

## 解決の柱

設計判断は四点ある。それぞれ三つのレンズ（長期視点・型安全性・
根本原因）に照らして以下のように決めた。

### 配置 — plan のオプション flag

`carina plan` に `--check-iam` / `--strict-iam` を追加する。デフォルトは
off。新しい `carina iam-check` サブコマンドにはしない。

* plan 本体の概念純度（plan は読み取り snapshot で副作用なし、
  ロックも取らない）を壊さないために、デフォルト off にする。flag が
  off の経路は今までと完全に同じふるまい。
* on のときは plan が STS と IAM への読み取り API を呼ぶ。これは plan
  の TOCTOU ウィンドウ（state の T0 fingerprint → T1 再読込）の外側で、
  plan 計算後・display 前の追加ステップとして実行する。state lock は
  従来通り取らない。
* on/off は CLI flag の型シグネチャに現れる。新サブコマンドを増やすと
  CI 側のワークフローを書き換えてもらう必要が出るので、既存 `plan`
  の素直な拡張で済ませる。

### 解決方式 — Simulate を主、ポリシー解析を fallback

`iam:SimulatePrincipalPolicy` を主の判定経路にする。actor は
`sts:GetCallerIdentity` で取れる ARN で固定する。awscc provider は
既に init 時に GetCallerIdentity を一度叩いて account_id を取って
いるので、同じ呼び出しから ARN フィールドを取り回せばよい。
provider 設定の `assume_role` がある場合も、SDK の credential が
assumed role 側になるため、GetCallerIdentity は assumed role の ARN
を返す。actor 決定は一回の STS 呼び出しで完結する。CloudControl 自
身は `RoleArn` パラメータを取れるが、carina の awscc provider は
これを使っておらず、handler は actor の権限で動く。

ただし Simulate で完全に判るわけではない。以下を honest に認める:

* **SCP (Organizations Service Control Policy) は見えない。**
  `SimulatePrincipalPolicy` の評価対象は identity policy と
  permission boundary と resource policy で、SCP は含まれない。SCP
  まで含める API は AWS には存在しない。SCP で deny されている権限
  は事前検知できない。
* **条件式は `ContextEntries` 次第で false negative になる。** condition
  key を明示的に渡さない条件式は permissive 扱い（通る）になる。
  CloudControl handler が内部で何の condition key をセットして呼ぶ
  か documented されていない以上、ここで false negative（実際には
  deny されるが Simulate では allow と判定）が出る。
* **Resource ARN scope は handlers list が持っていない。** registry
  schema の `handlers.<op>.permissions` は action 名だけで、対象
  resource ARN は書かれていない。`ResourceArns: ["*"]` で Simulate
  すると、`Resource: "arn:aws:foo::*:bar/*"` のような特定 ARN
  scope の grant しか持っていない actor が誤って allow 判定されう
  る（false positive、実際には CloudControl が触る resource が grant
  対象外で deny される）。

それでもこの check は価値がある。Issue の motivating example
（`ec2:DescribeInternetGateways` と `iam:CreateServiceLinkedRole`
が apply role の inline policy に完全に欠けていた）はどちらも
「action 完全欠け」のクラスで、Simulate が `["*"]` resource scope
であっても確実に拾える。Issue 本文「even with careful reading of the
registry resource type's create-handler section before authoring the
deploy role grants, two of the permissions were missed」が指す主たる
失敗モードはこれ。残る穴（SCP / 条件式 / resource scope）は別レイヤ
で対処されるべきで、本 check の "対象外" として明示する。

fallback の document 解析は Simulate が strict superset というわ
けではない。actor が `iam:SimulatePrincipalPolicy` を持たない環境
向けの最低限の代替で、`GetCallerIdentity` →
`GetRolePolicy` / `ListAttachedRolePolicies` で identity policy だけ
取って action 名マッチに落とす。boundary / SCP / 条件式 / resource
scope はどれも見ない。fallback 経路に入ったときは warning に
「Simulate より弱い判定です」旨を添える。

### スコープ — Provider trait に申告メソッドを生やす

provider 境界に `required_permissions(resource_type, op)` を新設する。

* awscc は registry schema の `handlers.<op>.permissions` を codegen 時に
  抽出して埋め込み、この trait method から返す。
* aws は現時点では空 Vec を返す。SDK の operation model に IAM action
  メタが付いていないので、別 issue で扱う。trait method を持つこと
  自体が将来 aws を埋める入口になる。
* host は plan の各 effect から provider と resource type を引き、
  trait を経由して必要 action 群を集める。

CFN schema を carina-cli が直接 HTTP fetch する案は捨てた。理由は二
つ。第一に awscc 専用のロジックが core/cli に入ってしまい、aws を埋
める将来の設計が破壊される。第二に「provider が知っている情報を
host が使えていない」のが Issue の根本動機で、host fetch は同じ穴を
別経路で埋めるだけで構造が変わらない。長期視点・型安全性・根本原因
の三レンズが揃って trait 拡張を選ぶ。

trait 拡張は WIT 境界の変更を伴う。`carina-plugin-wit` /
`carina-plugin-host` / `carina-plugin-sdk` / `carina-provider-mock` /
`carina-provider-aws` / `carina-provider-awscc` の協調更新が必要で、
本リポ単独 PR では完結しない。これは avoidable な複雑さではなく、
trait 境界という設計判断そのものに付随するコスト。

### 厳格度 — デフォルト warning、`--strict-iam` で error

`--check-iam` が指す既定挙動は「不足を stderr に警告として出すが
plan は成立させる」。`--strict-iam` を加えたときに限り、不足が一つ
でもあれば exit code を 1 にして plan を成立させない。

* registry schema の handlers は不完全な resource type がある
  （Issue 本文 "Out of scope" で言及）。デフォルトを error にすると
  false-positive で apply ブロックが起き、運用負荷が大きい。
* warning だけだと CI で見落とされる。CI では `--strict-iam` を
  明示 ON にしてもらえばよい。
* strict / non-strict は flag の型シグネチャに現れるので、convention
  ではなく型で扱われる。

## Provider trait の形

`carina-core::Provider` に同期メソッドを足す:

```rust
pub trait Provider {
    // 既存メソッド ...

    /// この provider が `resource_type` を `op` 操作するときに
    /// 必要な IAM action の列。判らない場合は空 Vec を返す。
    fn required_permissions(
        &self,
        resource_type: &str,
        op: PlanOp,
    ) -> Vec<String>;
}
```

`PlanOp` は plan が effect から導く operation を表す型で、新設する:

```rust
pub enum PlanOp {
    Create,
    Read,
    Update,
    Delete,
}
```

`Effect::Replace` は `Delete` と `Create` の二回問い合わせる。
`Effect::Remove` / `Effect::Move` / `Effect::Wait` は state 操作の
みで provider に触らないので問い合わせない。`Effect::Import` は
read として扱う。

引数を `&str` ではなく型でくくる選択肢もあるが、resource type 文字列
は plan 内部で既に `String` として流れているので、ここを newtype で
くくると plan 全域の波及になる。本 PR では `op` だけ enum 化し、
resource_type は文字列で受ける。将来 `ResourceType` 型を導入するなら
そのタイミングで一緒に締め直す。

戻り値が空 Vec の場合、host 側は「この provider はこの resource type
について情報を持たない」と扱い、warning も error も出さない。
`Option<Vec<String>>` で「申告なし」と「申告したが空」を区別する案も
あるが、IAM action list が空である意味のある状況は実質ないので
（無権限で実行できる API は存在しない）、Vec 一本でよい。

WIT 境界（`carina-plugin-wit/wit/provider.wit`）にも対応する
`required-permissions` を生やす。戻り値の `list<string>` は WIT の
標準型で表現できる。

## 配線

plan は `commands::plan::run_plan` の中で、plan 計算が終わって display
する直前に `iam_preflight` モジュールを呼ぶ。

1. 各 `Effect` から `(provider_handle, resource_type, PlanOp)` を取る。
2. provider ごとに `required_permissions` を集約する。
3. 集約済みの action 集合を `iam:SimulatePrincipalPolicy` に渡す。
   - actor の ARN は STS `GetCallerIdentity` で取る。
   - Simulate の権限が無い場合は fallback の document 解析経路に
     落ちる。
4. 結果を effect 単位で攻略し、不足 action があれば
   `resource (op) → 不足 action` の形で warning（または strict 時 error）を
   出す。warning の中身に「SCP/boundary を fallback では見落とす」
   旨を含めるかは fallback 経路に入ったかで切り替える。

`--out` で saved plan を書く場合の挙動は drift 警告と同じ扱いにする。
警告は stderr に出すが、plan ファイルは書く。`--strict-iam` で error
扱いになった場合は plan ファイルも書かない（exit 1）。

## 対象外（この check では拾えないもの）

設計判断「解決方式」のところに書いた制約を以下に再掲する。実装と
ユーザー向け表示でこれらを silent にしない:

* SCP で deny されているケース。
* condition key 依存の deny。
* handler が触る resource ARN scope で deny されているケース。
* CFN registry schema 自身が permissions list を欠いている / 不完全な
  resource type。Issue 本文 "Out of scope" で言及済み。
* awscc 以外の provider（aws namespace）。Issue 本文 "Out of scope"。

これらは Issue#3496 が指す主たる失敗モード（apply role に action
が完全に欠けている）ではない別クラスの問題で、本 check の枠外。

## 反対側の検討

* **新サブコマンド `carina iam-check`**: plan の概念純度を保てるが、
  CI 側で plan の後段に組み込んでもらう運用負荷を新たに作る。デフォ
  ルト off で plan に同居できるなら、ユーザーが知らない新コマンドを
  足すより既存 plan の素直な拡張のほうが採用障壁が低い。
* **plan 時に常時実行（flag なし）**: plan が必ず IAM/STS を叩くこと
  になり、plan の "credential なしでも読める snapshot" の性質が消える。
  ローカル開発で credential を持たないユーザーの plan が壊れる。
* **CFN schema を host から直接 fetch**: 短期コストは安いが awscc 専
  用のロジックが core/cli に入る。aws provider の将来対応で設計が破
  綻し、root cause（provider が知っている情報を host が使えていない）
  も塞げない。
* **`--check-iam` だけで error 扱い**: registry schema 不完全性に対
  する保険がない。schema が古い resource type で apply 不能になる。

## 後続 PR チェーン

本 design doc が merge された後の実装 PR は以下の順で進める。本 doc
を merge する PR には実装の差分を含めない（CLAUDE.md「Design PR
must merge before implementation PR」）。

1. **carina (本リポ)** — core trait + WIT + host + sdk + mock の追加、
   `PlanOp` 新設、CLI flag 追加（flag は受け取るが provider が全部
   空 Vec を返すので no-op）、design doc 参照を doc に書き足す。
2. **carina-provider-awscc** — codegen で `handlers.<op>.permissions`
   を抽出して埋め込み、`required_permissions` を実装。
   `generate-schemas.sh` と `generate-docs.sh` をペアで実行（CLAUDE.md
   「Codegen delegations always include the docs regen step」）。
   carina rev は (1) merge 後に bump。
3. **carina-provider-aws** — `required_permissions` を空 Vec で実装。
   carina rev は (1) merge 後に bump。
4. **carina (再び)** — provider rev bump。`iam_preflight` モジュール
   本体を実装（Simulate / document fallback / aggregation / 表示）。
   `aws-sdk-iam` / `aws-sdk-sts` を carina-cli に追加。warning と
   strict mode の表示分岐、stderr/stdout の振り分け、`--out` との
   相互作用、テスト。Issue は (4) のマージでクローズする。

## Acceptance（本 PR）

* 本 design doc が `notes/specs/` に存在し、配置・解決方式・スコープ・
  厳格度・provider 境界の形が固定されていること。
* 後続 PR チェーンの順序と各 PR の責務が doc 内に書かれていること。
* 反対側の選択肢を捨てた理由が doc 内に書かれていること。
