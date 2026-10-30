# `DeferredValue`を段階で分ける設計

<!-- refines ./2026-05-23-gradual-typing-design.md#6-4 -->

Tracking: carina-rs/carina#4094

[gradual typingの設計](./2026-05-23-gradual-typing-design.md)のPhase 2.cです。現状は[「未確定」の形の現状](./2026-10-29-unknown-forms-current-state.md)にまとめてあります。この文書は、main（12b95e27）のコードを調べたうえで、`DeferredValue`をどう分けるかと、for-loopの理由をどこに置くかを決めます。

## 何が問題か

`Value`は`Concrete(ConcreteValue) | Deferred(DeferredValue)`で、`DeferredValue`の中に段階の違うものが同居しています。

- パーサーの中でしか存在してはいけない仮置き。関数の引数（`FnParam`）、関数内のlet（`FnLocal`）、moduleの引数（`ModuleArgument`）、空の`${}`（`EmptyInterpolation`）です。前の3つは関数の呼び出しやmoduleの展開で置き換わり、最後の1つは検証で拒まれます。
- for-loopのテンプレートの中の仮置き（`ForKey`、`ForIndex`、`ForValue`）。子の資源を作るときに置き換わります。繰り返す対象がplan時に分からないと、テンプレートのまま保存したplanに入り、applyの途中で置き換わります。
- planやapplyまで残ってよい「まだ分からない」の印（`UpstreamExport`、`UpstreamAllExports`、`PendingReRead`）。
- 評価すれば具体的な値になる参照と式（`ResourceRef`、`BindingRef`、`Interpolation`、`FunctionCall`、`ResolvedFunctionCall`、`Access`、`Coalesce`、`Unavailable`）。

型が同じなので、置き換え損ねたパーサーの仮置きは、planにもstateにもproviderへの境界にも、型の上では届いてしまいます。今それを止めているのは、境界での実行時の拒否（`UnknownNotAllowed`や`assert_resource_fully_resolved`）だけです。

## 方針

仮置きを、値の型の型引数にします。

```rust
pub enum Value<P = NoPlaceholder> {
    Concrete(ConcreteValue<P>),
    Deferred(DeferredValue<P>),
}

pub enum DeferredValue<P> {
    // 参照と式（今と同じ）
    ResourceRef { .. }, BindingRef { .. }, Interpolation(..), FunctionCall { .. },
    ResolvedFunctionCall { .. }, Access { .. }, Coalesce { .. }, Unavailable { .. },
    // planやapplyまで残ってよい印
    Unknown(ApplyTimeUnknown),
    // 段階ごとの仮置き
    Placeholder(P),
    // 秘匿の印（この設計では動かさない）
    Secret(..), RedactedSecret(..),
}

pub enum NoPlaceholder {}                       // 値を持たない型
pub enum ParserPlaceholder { FnParam { .. }, FnLocal { .. }, ModuleArgument { .. }, EmptyInterpolation }
pub enum LoopSlot { Key, Index, Value }
pub enum ApplyTimeUnknown { UpstreamExport { .. }, UpstreamAllExports { .. }, PendingReRead(..) }
```

`NoPlaceholder`は値を1つも持たない列挙型です。そのため、`Value<NoPlaceholder>`の`Placeholder`の腕は中身を作れず、`match never {}`だけで閉じられます。展開が終わった後の値（既定の`Value`）は、パーサーの仮置きもfor-loopの仮置きも、型の上で持てません。

段階と型の対応は次のとおりです。

| 段階 | 値の型 | 持てる仮置き |
| --- | --- | --- |
| パーサーの出力、関数の本体、moduleの定義 | `Value<ParserPlaceholder>` | 関数の引数、関数内のlet、moduleの引数、空の`${}` |
| for-loopのテンプレート（`DeferredForExpression`） | `Value<LoopSlot>` | for-loopのキー、番号、値 |
| 展開と検証が終わった後（資源、データソース、plan、state、provider） | `Value`（`Value<NoPlaceholder>`） | なし |

### 段階の境目

パーサーの段階から展開後の段階への変換は、失敗しうる関数1つにまとめます。

```rust
impl Value<ParserPlaceholder> {
    fn into_expanded(self) -> Result<Value, LeftoverPlaceholder>;
}
```

moduleの展開と関数の呼び出しで仮置きを置き換えたあと、ディレクトリの設定を検証済みにするところ（今の`ResolvedFile`を作るところ）で、この変換を通します。残っていた仮置きは`LeftoverPlaceholder`という型付きのエラーになります。空の`${}`は置き換わることがないので、今の`validate_no_empty_interpolations`の役目もこの変換が引き継ぎ、同じ文言のエラーを出します。

for-loopの子を作る`expand_deferred_children`は、`Value<LoopSlot>`のテンプレートを受け取り、仮置きを置き換えた`Value`を返す変換になります。テンプレートの外の値は、最初から`Value`です。

### for-loopの理由をどこに置くか

案は4つありました。

- パーサーの仮置きと同じ型に入れる。テンプレートは保存したplanに入るので、「パーサーの中だけ」にはなりません。
- planやapplyまで残る印（`ApplyTimeUnknown`）に入れる。変更は一番小さいのですが、ふつうの資源の属性がfor-loopの仮置きを含まないことを、型で保証できません。
- テンプレート専用の型（`Value<LoopSlot>`）に入れる。テンプレートの中にだけ現れ、子を作るときの変換で必ず消えます。
- 参照と式（`ReferenceExpr`）に入れる。設計文書の§6.4の考えに近いのですが、保存したテンプレートの中でだけ許す、という条件を別に持たせる必要があります。

テンプレート専用の型を選びます。長い目で見ても、型の安全性でも、根本原因の場所でも、この案だけが「ふつうの値はfor-loopの仮置きを持たない」を型で保証するからです。§6.4は、置き換えを参照の解決の一部と見ていました。しかし今のコードでは、置き換えはテンプレートから子を作る処理です（更新のあとのplanとapplyの途中の両方で使います）。§6.4はこの点で古くなっています。

### 保存したplanの形式

テンプレートの中の仮置きは、JSONでは今と同じ形のまま書けます。それでも、`Unknown`の理由の一覧から仮置きを外すので、読み込みの型が変わります。このプロジェクトは後方互換を持たないので、plan形式の版を1つ上げます。読み込みで、テンプレートの外に仮置きが現れたら、型付きのエラーにします。

### 関数の本体

`ResolvedFunctionCall`は、呼び出す関数（`UserFunction`）を持っています。その本体には、関数の引数と関数内のletの仮置きが入っています。本体は値そのものではなく関数の定義なので、`UserFunction`の本体は`Value<ParserPlaceholder>`のままにします。呼び出しを評価して得られる値だけが`Value`です。

## 消える実行時の拒否

型で持てなくなるので、次の分岐はなくなります。

- providerへの変換（`value_to_json`、WITへの変換）とstateへの書き出しで、パーサーの仮置きとfor-loopの仮置きを拒んでいる分岐。
- `assert_resource_fully_resolved`と、executorの`assert_fully_resolved`のうち、仮置きを見つける部分。
- `validate_no_empty_interpolations`（境目の変換に移ります）。

参照や式、planやapplyまで残る印は、境界に届くことがあり得ます。たとえばupstreamの値は、applyでもう一度解決するまで分かりません。なので、これらに対する拒否は残します。`deferred_populate`の確認は、「待つ指定のない参照を拒む」という意味の検証なので、この変更では消えません。参照を数える部分だけ、新しい型に合わせて書き直します。

## この設計で扱わないこと

`Secret`と`RedactedSecret`は、未確定とは別の軸の印です。#4094では列挙に挙げましたが、この設計では動かしません。調べると、`RedactedSecret`を「解決済み」とみなす処理（`is_fully_resolved`）と「まだ分からない」とみなす処理（型の検査、未解決の参照の除外、for-loopの展開）が混在していました。秘匿の値をどう扱うかを1か所で決める必要があるので、別のissue（#4140）で扱います。

provider側（carina-provider-aws、carina-provider-awscc）は、`DeferredValue`と`UnknownReason`をテストでしか直接使っていません。本番のコードはprotocolの型を通しているので、この変更では影響を受けません。テストの直しと、新しいrevへの更新は必要です。

## 段取り

1. この設計メモを入れる（文書だけのPR）。
2. `Value`に型引数を足し、`ParserPlaceholder`、`LoopSlot`、`ApplyTimeUnknown`に分ける。パーサー、moduleの展開、for-loopの展開、検証の境目、保存したplanの形式を1つのPRで変える。仮置きの拒否の分岐を消し、型で持てないことをcompile_failのdoctestで示す。
3. provider側の2つのrepoのテストを直し、revを上げる。

## 現状メモの訂正

[「未確定」の形の現状](./2026-10-29-unknown-forms-current-state.md)にあった次の2点の誤りは、この設計と同じPRで直しました。

- `UnknownReason`の数は11ではなく10です。
- `PostCreateReadIncomplete`は、今は`PendingReRead`です。理由を3つ（読めたが不完全、読めなかった、見つからなかった）持ち、次のapplyではなく次のrefreshで消える印です。
