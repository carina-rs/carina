# carina#3368 実装計画 — ドット付きカスタム型の解決

<!-- derived-from #issue-3368 -->

issue #3368（#3239 の続編）。同一根本原因による2つのカスタム型解決ギャップを潰す。
本ドキュメントは Codex が書いた初版計画に Opus のレビュー指摘（3点 + bootstrap×型完全化の深掘り）を統合したもの。実装は Codex に委譲、Opus はレビューのみ。

## 2つのバグ（同一根本）

1. **再現1（出口なしループ）**: モジュール引数の型を snake_case で書くと
   （`bad_arg: iam_policy_arn`）、パーサが `snake_to_pascal` で機械的に
   `IamPolicyArn` を提案するが、その `IamPolicyArn` も未登録で
   「unknown custom type 'IamPolicyArn'」と弾かれる。本物の登録識別子は
   ドット付き `aws.iam.Policy.Arn`。提案に従っても解決できない。
2. **再現2（ドット付き未知型の素通り）**: `list(aws.iam.TotallyFake.Arn)`
   のような未登録ドット付き型が `carina validate` を通る。

## 根本原因

文法 `type_ref`（`carina-core/src/parser/carina.pest:125-126`）→
`parse_type_expr_atom` の `Rule::type_ref` アーム
（`carina-core/src/parser/types.rs:191-221`）。3+セグメントで末尾 PascalCase
なら `config.is_schema_type(...)` で `TypeExpr::SchemaType` に分類、外れたら
**無条件に `TypeExpr::Ref` にフォールバック**。だが `schema_types` は CLI/LSP
とも本番で空（`Default::default()`）、プロバイダも `register_schema_type` を
呼ばない → `SchemaType` 分類は実質デッド。ドット付きカスタム型は実運用で
必ず `Ref` に落ち、全消費側（`functions.rs:328`、
`module_resolver/typecheck.rs:234`、`validation/mod.rs:837` walk）が
「文字列形なら OK、照合なし」で素通しする。これが silent-accept の正体。
実際の登録済み識別子は `ProviderContext.validators:
HashMap<TypeIdentity, ValidatorFn>` にある。

## 確認済み不変条件

- `iam_policy_arn()`/`iam_role_arn()` は生成スキーマで実使用 →
  `collect_custom_type_validators` が `aws.iam.Policy.Arn`/`Role.Arn` を
  `validators` に登録。`aws.iam.OidcProvider.Arn` は awscc 側。
- **WASM 実運用でも identity は保持**（#3364 のような落下なし）: proto の
  `Custom{name}` → host `wasm_convert.rs` で `TypeIdentity::from_dotted(name)`
  → SchemaRegistry。
- validate 時 host 側で `ProviderContext.validators.keys()` から全登録済み
  カスタム型 TypeIdentity を列挙可能。リソース種別は `SchemaRegistry::iter()`
  / `has_managed()` / `has_data_source()`。

## 前提条件（不変条件への依存・明記）

この照合は「`validators` のキー集合 = ロード済みプロバイダの全登録済み
カスタム型 identity」という不変条件に依存する。`custom_type_validator`
（WASM 動的ファクトリ）は**値検証専用**であり、型存在判定には使わない。
将来この不変条件が崩れる（validators に入らず custom_type_validator だけに
登録される型が出る）と、walk が実在型を誤って未登録判定する。その時は
照合ソースの見直しが必要。

## bootstrap 制約（型完全化の核心論点）

CLI の validate ではルートファイルの初回パースが bootstrap context
（`customs_loaded=false`, validators/種別 空）で行われる
（`load_configuration_with_config`）。`module_resolver::resolve_modules_with_config`
が enriched context で再パスするのは **imported module だけ**で、caller 自身
（root config）の arguments/attributes/exports は再パスされず bootstrap の
AST がそのまま使われる。だから #3239 は root config 用に post-parse walk
（`validate_argument_custom_types` を enriched context で実行）を別途置いた。

→ **分類確定をパーサ時点に置く素朴な型完全化は成立しない**（bootstrap では
validators が空で実在ドット型を分類できない）。型完全化するなら「未解決
ドット型」を表す中間状態を許し、enriched context が確実に届く**解決ステップ
（resolver newtype / typestate）を必ず通さないと消費できない型**にする
（未解決状態を消すのではなく、未解決は解決関数を通さねば消費不能にする）。
root config も imported module も同じ解決ステップを通る一点に寄せる。

## タスク

### タスク0（済）: 失敗する再現テスト（TDD Red）

`carina-cli/tests/validate_unknown_custom_type_e2e.rs` に追加済み・Red 確認済み:
- `validate_rejects_fake_dotted_custom_type`（FAIL=再現2）
- `validate_snake_case_suggests_dotted_registered_identity`（FAIL=再現1）
- `validate_accepts_registered_dotted_custom_type`（PASS=過剰拒否ガード）

test factory に `aws.iam.Policy.Arn` の identity 付き Custom 型を登録済み。

### タスク1: 型完全化の半径測定（最優先・実装形態を決める）

`TypeExpr` に「未解決ドット型 vs 解決済み型」の区別を入れる（または `Ref` を
resource-kind 専用に絞り、ドット付きカスタム型は必ず解決ステップ経由でしか
得られない）スタブを当て、消費側 match（`functions.rs:328/455`,
`typecheck.rs:234`, `validation/mod.rs:837`, `module.rs:887/911/1083`,
`ast.rs` Display, LSP `top_level.rs:471/474`）が何箇所コンパイルエラーに
なるか測る:

```sh
cargo check --workspace --all-targets 2>&1 | grep -c "^error"
```

revert。**判断分岐**:
- 半径が小さい（単〜低2桁、parser/validation/resolver に閉じる）→
  **in-PR で型完全化**（タスク2を型版で実装）。第一候補。
- 半径が大きい → タスク2を runtime walk 版で実装 + **同一 PR レスポンスで
  follow-up issue**（測定値・error 数・affected files・目標 type signature
  を明記。「後で」は禁止）。

### タスク2: 未登録ドット型の拒否（根本対応の本体・形態はタスク1で分岐）

**型版（第一候補）**: パーサは「未解決ドット型」を中間状態として作り、
root config / imported module の両方が必ず通る解決ステップ（enriched context
で validators・種別レジストリ照合）を resolver newtype/typestate で必須化。
消費側は解決済み型しか受け取れない＝新消費者が照合を忘れても silent-accept が
型構造上不可能。

**runtime 版（フォールバック）**: `collect_unknown_simple_types_in`
（`validation/mod.rs:799-842`）が今 `Ref`/`SchemaType` をリーフ扱いで
スキップしているのをやめ:
- `SchemaType{provider,path,type_name}` → `TypeIdentity::from_schema_type`
  で identity 化し `validators` 照合、未登録なら拒否。
- `Ref(ResourceTypePath)` → リソース種別レジストリ照合 + validators 照合
  （ドット付きカスタム型は実運用で Ref に落ちるため）。どちらにも無ければ拒否。
- enriched context のみで走る（CLI:271 / LSP:226 ゲート）→ bootstrap 誤検知なし。

いずれの形態でも `aws.vpc`（2セグ種別参照）と `aws.iam.Role.Arn`
（validators 登録済み）が壊れないこと。

### タスク3: ProviderContext にリソース種別レジストリを追加

`ProviderContext`（`carina-core/src/parser/config.rs`）に
`resource_types: HashSet<(String,String)>` + `has_resource_type()` を追加。
`enrich_provider_context`（`carina-cli/src/commands/mod.rs:129`）で
`SchemaRegistry::iter()` から充填。LSP の enriched context 構築
（`carina-lsp/src/diagnostics/mod.rs:96-110`）でも同じ helper で充填
（片側だけ更新される再発を防ぐため共通 helper に寄せる）。`schema_types` は
責務を明確化（リソース種別判定には使わない）。

### タスク4: 提案ロジックを validators ベースへ集約（再現1）

機械的 `snake_to_pascal` 提案を、登録済み `TypeIdentity` 集合への正規化照合に
置換。**正規化方針（snake↔dotted を吸収）**:
1. 入力（`iam_policy_arn` / `IamPolicyArn` / `iamPolicyArn`）を snake 正規形に
   落とす（`pascal_to_snake` 等）。
2. 各登録 identity を比較キーに落とす:
   - 第1段: identity 全体の snake 形（例 `aws.iam.Policy.Arn` →
     `iam_policy_arn` 相当: segments+kind を snake 連結。provider 軸は
     落として比較してよい）と入力 snake 形の**完全一致**。
     → `iam_policy_arn` が `aws.iam.Policy.Arn` にここでヒットする
     （segments=`[iam,Policy]`+kind=`Arn` → `iam_policy_arn`）。
   - 第2段: `TypeIdentity.kind` の snake 形（`arn`）と入力末尾の一致。
   - 第3段: 第1段キーへの編集距離が閾値内の最近傍。
3. 候補は必ず `validators` に存在する完全識別子（Display 形のドット付き）。
   近傍なしなら提案を付けない（誤提案ゼロ）。

ベア名失敗の両経路（パーサ `types.rs:163-169` の snake_case 拒否アーム +
unknown custom type アーム、post-parse walk）で同じ共通ヘルパを使う。
提案文言は `aws.iam.Policy.Arn` を含み、かつ `IamPolicyArn`（ベア名）を
含まないこと（再現テストの assert）。

### タスク5: 追加テスト

- **LSP parity**（`carina-lsp/src/diagnostics/`）: 同じ未登録ドット型が
  CLI と LSP 両方で警告。提案も同一識別子を指す（#3239 と同じ二重テスト）。
- **近傍なしで誤提案が出ない**ケース（`aws.foo.TotallyMadeUp.Xyz`）。
- **Ref 種別参照 `aws.vpc` が壊れない**ケース（正規のリソース種別参照）。
- **型完全化したらコンパイル fail ガード**（未解決型を解決ステップ無しで
  消費する擬似コードがコンパイルできないことを示す test/doc）。

### タスク6: real-infra smoke

`infra/modules/github-oidc` 等（`aws.iam.OidcProvider.Arn` /
`aws.iam.Policy.Arn` / `aws.iam.Role.Arn` 実使用）に対し `carina validate`
が通ること（AWS 認証不要の静的範囲）。fixture コピーで
`aws.iam.TotallyFake.Arn` に差し替えると失敗すること。
※実 infra への AWS 接触コマンドはユーザー駆動。

## 検証ゲート

```sh
cargo check -p carina-core && cargo check -p carina-cli && cargo check -p carina-lsp
cargo nextest run $(scripts/touched-crates.sh)   # carina-core 触るので最終は --workspace
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-*.sh
cargo nextest run --workspace --all-features      # feature-gated snapshot
```

## root-cause 自己チェック

- 「新しい消費側が明日現れたら再発するか?」を**型シグネチャ**で答える:
  型版なら未解決ドット型を構築する公開 constructor が無く、解決ステップを
  通らないと消費できない＝再発不可。runtime 版なら walk を経由しない消費側で
  再発しうる＝follow-up で型化する旨を PR と issue に明記。
- 提案ロジックは1ヘルパ・レジストリ唯一ソースに集約され、カスタム型が
  増減しても陳腐化しない。
