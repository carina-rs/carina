# 「未確定」の形の現状（2026-10-29時点）

<!-- refines ./2026-05-23-gradual-typing-design.md#1-1 -->

Tracking: carina-rs/carina#3212

carinaには、「今はまだ決まっていない」を表す形がいくつもあります。値がまだ評価できない、型が静的に分からない、作成した資源の属性がまだ読めていない、などです。[gradual typingの設計](./2026-05-23-gradual-typing-design.md)は、これらを整理して型で区別する道筋を描きました。#3212はその最初の段階（Phase 1）で、それぞれの形を1か所で説明することが目的です。

この文書は、main（948e9c45）のコードと突き合わせた現状をまとめます。設計文書の§1.1の表は、書かれた後にコードが変わり、一部が古くなっています。どこが変わったかは最後の節にまとめました。コードの位置は、行番号ではなく型と関数の名前で示します。

## 一覧

| 形 | 層 | 意味 | 作るところ | 検証での扱い |
| --- | --- | --- | --- | --- |
| `DeferredValue`の参照と式（`ResourceRef`、`BindingRef`、`Interpolation`、`FunctionCall`、`ResolvedFunctionCall`、`Access`、`Coalesce`） | 値 | 評価すれば具体的な値になる式。参照先がまだ決まっていない | パーサー | スキーマの検証は飛ばす。providerやstateに渡す境界では拒む |
| `DeferredValue::Unavailable` | 値 | 組み込み関数を評価しようとして失敗した呼び出し。待っている仕事ではない | 参照の解決（寛容なモード） | 同上。保存するときは`FunctionCall`に戻す |
| `DeferredValue::Unknown(UnknownReason)` | 値 | plan時には分からない値。理由が11種類ある（次の節） | 理由ごとに違う | 飛ばす。境界では`UnknownNotAllowed`で拒む |
| `DeferredValue::Secret`、`RedactedSecret` | 値 | 秘匿の印。未確定とは別の軸 | `secret()`、stateと保存したplanの読み込み | `Secret`は中を検証する。`RedactedSecret`はパーサーからは作れない |
| `TypeExpr::Unknown` | 型（静的） | 健全な静的な型が付けられないので、型の検査をしない | 3か所（後述） | 型の検査を省き、存在だけを確かめる |
| 絞り込みの型の`MetadataOnly(None)` | 型（静的） | 名前（`TypeIdentity`）を持たない型。素の`String`か、名前のない絞り込み | スキーマのbuilder | 検査できる普通の型。名前のある型へは代入できない |

このほかに、値ではないが「まだ分からない」に見えるものが3つあります。

- planの「(known after apply)」は値ではありません。利用者が書いていない読み取り専用の属性について、表示の行（`DetailRow::ReadOnly`）が出す文字列です。
- 作成途中の資源（#3909）は、資源の状態`Current::CreateInFlight`です。属性を持たないので、未確定の値は作られません。
- `PendingToken`は、資源のidentityがまだ決まっていないことを表す、プロセスの中だけの印です。値の未確定ではありません。protocolとplugin-hostにある同じ名前の型とは別物です。

## `UnknownReason`の11種類

`DeferredValue::Unknown`は1つの型ですが、理由ごとに生まれる段階も消える段階も違います。

| 理由 | 作るところ | いつ消えるか | 表示 |
| --- | --- | --- | --- |
| `UpstreamExport`、`UpstreamAllExports` | planの参照の解決（`stamp_unresolved_upstream`）。upstream stateから解決できなかった参照だけ | upstreamのapplyの後 | `(known after upstream apply: …)` |
| `ForKey`、`ForIndex`、`ForValue` | for式の展開 | iterableが解決したとき | `(known after upstream apply: …)` |
| `FnParam`、`FnLocal` | 利用者定義の関数の解析 | 関数を呼んで置き換えたとき | `(unresolved function …)` |
| `ModuleArgument` | moduleの引数の解析 | moduleの展開で置き換えたとき。providerに向けた検証より前 | `(unresolved module argument: …)` |
| `EmptyInterpolation` | `${}`の解析 | 消えない。書きかけの印で、発生した場所で診断を出す | `(empty interpolation)` |
| `PostCreateReadIncomplete` | stateの読み込み後、planの入力を作るとき | 次のapplyで読み直したとき | `(known after next apply: …)` |

`PostCreateReadIncomplete`は#4084で入りました。作成は成功したものの、その後の読み取りで資源が見つからなかったか読めなかった場合（`CreateObservation::Unobserved`）、stateには値ではなく`PartialReadMarker`という印が記録されます。部分的に成功した作成や更新も同じです。planの入力を作るとき（`materialize_partial_read_unknowns`）、その印の付いた属性がこの理由の未確定の値として組み立て直されます。stateに未確定の値そのものが保存されることはありません。差分の計算は、この値を含む望ましい値に向けては変更を作りません。

`Unknown`は、何とも等しくなりません。同じ理由の`Unknown`どうしでもです。例外は`ModuleArgument`どうしです。ディレクトリの解析を不動点まで繰り返すときに、同じ置き場所だと分かる必要があるからです。

## `TypeExpr::Unknown`を作る3か所

`TypeExpr::Unknown`のdocコメントは「apply_inferenceだけが作る」と書いていましたが、今は3か所から作られます。

1. exportの型の推論（`infer_export_params`）。推論できなかったとき（`Ok(None)`）と、推論が失敗したとき（`Err`）です。失敗したときは、エラーも何番目のexportかと一緒に記録されます。スキーマが手に入った後の再試行（`retry_unknown_export_inference`）は、エラーを出し直しますが、値は`Unknown`のまま残します。
2. upstreamのexportを読むとき（`resolve_file_type_exprs_or_unknown`）。読む側のprovider contextでは名前を解決できない型を、`Unknown`に置き換えます。読む側にとって健全な制約がないからです。これは推論の失敗ではありません。
3. iterableの要素の型の置き場所（`List(Unknown)`、`Map(Unknown)`）。具体的なリストやマップの、要素の型を決めずに外側の形だけを表します。

使う側は、どこでも「型を検査しない」として扱います。型の検査はどんな値も通し（`typecheck`）、exportへの参照は型を比べずに存在だけを確かめます（`RefType::Unchecked`）。ただし、`Unknown`が推論された側の型のときは、具体的な型を期待する場所には当てはまりません。型を持たない土台への`.`や`[]`によるアクセスは、推論のエラーになります。

## 検証が何をするか

値の未確定について、検証はおおむね次のように振る舞います。

- スキーマの値の検証は、具体的でない値（`as_concrete()`が`None`）を飛ばします。具体的なリストやマップの中は要素ごとに見て、未確定の要素だけを飛ばします。リスト全体が未確定なら、リストとして通します。
- `Secret`と`Coalesce`は飛ばさず、中を検証します。
- 例外が1つあります。名前だけの参照（`BindingRef`）をenumの属性に書き、その名前がenumの別名と重なる場合は拒みます（#2978）。
- 静的な型の検査（`typecheck`）は、`ResourceRef`を`StringLiteral`以外のどの型にも通します。
- providerやstateへ渡す境界（`value_to_json`、WASMへの変換）は、未確定の値をすべて拒みます。apply直前の検査（`assert_fully_resolved`）も、`Secret`の中を除いて、未確定の値をすべて拒みます。
- 保存したplanは、未確定の値をそのまま持ちます。applyのときにfor式の型紙などを置き換えるためです。ただし`ResolvedFunctionCall`と、生の`Secret`は保存できません。

`deferred_populate`の属性（作成の後でproviderが埋める値）を、`wait`なしで参照すると、validateが拒みます（`validate_deferred_populate_refs`）。この検査が見るのは`ResourceRef`だけです。

## 型の名前の有無と代入

#3218の修正の後、`TypeIdentity`がないことは「区別しない、広い」という意味ではなくなりました。「広い」は、`TypeIdentity`の中の空の軸（`provider`が`None`、`segments`が空）が表します。代入（`is_assignable_to`）の決まりは次のとおりです。

- 名前のある型から名前のある型へ: 軸ごとの包含（`TypeIdentity::assignable_to`）と、長さの制約の包含を確かめます。受け取る側の空の軸は何でも受け入れます。
- 名前のない絞り込みから名前のある型へ: 拒みます。
- 絞り込みのない型から、名前のない受け取り側へ: 受け入れ、受け取り側のpatternと長さは具体的な値で確かめます（#3805）。
- それ以外の`String`どうし: patternを文字列として比べ、長さの制約の包含を確かめます。名前のある型から名前のない型への代入は、ここに入ります。

組み込みの型（`Ipv4Cidr`など）も、今は`TypeIdentity::bare`で名前を持っています。`TypeExpr::Unknown`はこの比較に来ません。静的な型を`AttributeType`に持ち上げるところで`None`になり、呼び出し側が型を検査しない参照として扱うからです。

## 統合するかどうかの判断

#3212は、重なっている形を1つにまとめられるかを判断することも求めています。判断は次のとおりです。

`TypeExpr::Unknown`と、名前のない絞り込みの型は、1つの`Dynamic`にまとめません。前者は「型を検査しない」という判断で、後者は「検査できる型の形」です。重なる場面がありません。設計文書のPhase 2.b（`CustomTypeOrigin`）は取り下げます。`AttributeType::Custom`はすでになく、組み込みの型も名前を持つようになり、#3218の健全性の修正も軸ごとの代入（`assignable_to`）として入ったので、前提がなくなりました。

`Value::Unknown`と`Value::Deferred`は、すでに1つの`DeferredValue`にまとまっています。残っている問題は逆向きで、1つの型に意味の違うものが入っていることです。参照と式は評価すれば具体的になり、`Unknown`はplan時には分からず、`Secret`は秘匿の印です。`UnknownReason`の中にも、パーサーの中でしか存在してはいけない置き場所（`FnParam`、`FnLocal`、`ModuleArgument`、`EmptyInterpolation`）と、planやapplyまで残る印（upstream、`PostCreateReadIncomplete`）が混ざっています。これを型で分けるのがPhase 2.cで、#4094で扱います。

`TypeExpr::Unknown`を取り除くPhase 2.aは、#4093で扱います。ただし、設計文書が考えていた「推論の戻り値を`Result`にする」だけでは足りません。今の`Unknown`は、推論の失敗、読む側では名前を解決できない型、要素の型の置き場所という3つの意味を持っているからです。それぞれに、`TypeExpr`の外の型付きの置き場所を用意する必要があります。#2597と#2598（upstreamのexportの推論の失敗を伝える）とも重なります。

## 設計文書との違い

[gradual typingの設計](./2026-05-23-gradual-typing-design.md)の§1.1は、次の点で今のコードと合いません。

- 6行目の`AttributeType::Custom { identity: None }`はありません。名前を持つかどうかは、絞り込みの型が持つ`RefinementBinding`（`MetadataOnly(Option<TypeIdentity>)`か、名前が必須の`Validator`）が表します。
- `None`の3つの意味のうち、組み込みの型はもう`None`ではありません。
- `TypeExpr::Unknown`は、`apply_inference`だけでなく3か所から作られます。
- `UnknownReason`には、表にない`FnParam`、`FnLocal`、`ModuleArgument`、`PostCreateReadIncomplete`があります。`DeferredValue`にも、`Coalesce`、`Unavailable`、`Access`、`ResolvedFunctionCall`、`RedactedSecret`が加わっています。

## 関連する文書

- [gradual typingの設計](./2026-05-23-gradual-typing-design.md)
- [作成の後で埋まる属性](./2026-05-14-deferred-populate-attribute-design.md)
- [部分的に成功した作成の結果](./2026-06-15-partial-create-outcome-design.md)
- [クラウドに届いてから失敗した作成](./2026-10-23-3909-create-reached-cloud-then-failed-design.md)
- [TypeExprの第2段階](./2026-05-03-typeexpr-stage2-design.md)
