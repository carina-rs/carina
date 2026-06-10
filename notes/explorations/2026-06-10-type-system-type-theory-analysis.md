# Carina 型システムの型理論的分析

2026-06-10 / 対象コミット: main `ad91ed00`

このレポートは、Carina の型システムを型理論の語彙で分析したものである。
実装の詳細仕様書ではなく、「Carina が型理論上のどの概念をどう使っているか、
どこに理論的な強みと弱みがあるか」を整理することを目的とする。

## 1. 全体像 — 三層の型システム

Carina の型付けは、性質の異なる三つの層が積み重なってできている。

| 層 | 担い手 | 検査時期 | 性格 |
| --- | --- | --- | --- |
| DSL 型注釈層 | `TypeExpr`(`parser/ast.rs`) | 実行時(validate / plan 時) | 構造的・漸進的 |
| スキーマ型層 | `AttributeType` / `AttrTypeKind`(`schema/mod.rs`) | 実行時(検証・差分・正規化) | 公称+構造のハイブリッド、篩型 |
| ホスト言語層 | Rust の newtype / typestate / 可視性 | コンパイル時 | 静的・線形的 |

重要なのは、対象言語(DSL)の値に対する型検査はすべて実行時に行われる一方、
「型システムの実装そのものが壊れた状態に陥らないこと」はホスト言語である
Rust の型システムでコンパイル時に保証している、という二段構えである。
つまり Carina は「動的型付き DSL」を「静的型付きメタ言語」で実装しており、
型理論的な工夫の多くはメタ言語側(第3層)に集中している。

## 2. 代数的データ型としての基盤

スキーマ型 `AttrTypeKind`(`carina-core/src/schema/mod.rs:473-557`)は、
型理論の標準的な型構成子をほぼ一通り備えている。

- 基底型: `String` / `Int` / `Float` / `Bool` / `Duration`
- 直積型: `Struct { name, fields }` — 名前付きフィールドのレコード型。
  `StructField.required` により各フィールドが `T` か `Option<T>` 相当かを表す
- 列・写像: `List { element_type, ordered }` / `Map { key, value }` —
  `ordered` フラグは「列(sequence)」と「多重集合(multiset)」を同じ構成子で
  表し分けており、等価性判定の意味論(§9)に効く
- 合併型: `Union(Vec<AttributeType>)` — タグなし合併(untagged union)。
  「いずれかのメンバが受理すれば妥当」という集合論的合併の意味論を持つ
- 列挙型: `Enum { identity, base, values, … }` — 基底型 + 値の有限集合 +
  公称的な identity。閉じた列挙(values あり)と開いた列挙(values なし、
  検証関数で判定)の両方を表せる

DSL 注釈側の `TypeExpr`(`parser/ast.rs:233-292`)はこれに加えて
`StringLiteral(String)` を持つ。これは値ひとつだけを持つシングルトン型で、
`Union` と組み合わせると `'dev' | 'prod'` のようなリテラル合併型が書ける。
TypeScript のリテラル型と同じ構図であり、ユーザー定義の列挙を
公称的な `Enum` を経由せず構造的に表現する手段になっている。

タグなし合併である点には理論的な代償がある。直和型(tagged sum)と違い、
値がどのメンバに属するかを判別子なしに構造照合で推定する必要があり、
実装は「最良スコアのメンバを選ぶ」ヒューリスティック
(`select_union_member`)に頼る。メンバ同士が重なる場合
(例: `Int | Float`)は数値の包摂(Int→Float 昇格)で吸収しているが、
重なりの大きい合併を定義すると正規化先が曖昧になる余地は残る。

## 3. 篩型(refinement types)としての制約付き基底型

Carina に「カスタム型」という独立した型構成子はない。代わりに
`String` / `Int` / `Float` の各変種が

- 正規表現パターン(`pattern: Option<String>`)
- 長さ・値域の区間(`length` / `range`、両端とも `Option` で開区間を表現)
- 任意の検証クロージャ(`validate: Arc<dyn Fn(&Value) -> Result<(), TypeError>>`)

を持ち運ぶ。これは型理論でいう篩型 — `{ x: String | P(x) }` のように
基底型を述語で絞り込んだ型 — の実行時実装である。区間とパターンは
決定可能な述語のクラスとして構造的に持ち(直列化もできる)、それを超える
述語は不透明なクロージャに逃がす、という二段構えになっている。

この設計の帰結が WASM 境界で現れる。構造的に持っている pattern / length は
プロトコルを越えて直列化できるが、クロージャは関数値なので直列化できない。
そのため境界の向こうの検証は `CustomTypeLookup`(`TypeIdentity` をキーに
ホスト側レジストリへ問い合わせる仕組み)という側道で実現している。
「篩の述語のうち一階の表現を持つ部分だけがワイヤを渡れる」という制限は、
篩型を動的言語で実装する際の典型的な分割線であり、carina#3364
(プロトコル境界で pattern が脱落した事故)はまさにこの分割線上で起きた。

## 4. 再帰型 — Ref + defs と二つの射影

CloudFormation の WAFv2 `Statement → AndStatement → List<Statement>` のような
循環スキーマを表すため、スキーマは

```rust
pub struct Schema {
    pub root: AttributeType,
    pub defs: BTreeMap<String, AttributeType>,   // 名前 → 定義
}
```

という形を取り、型の中に `Ref(String)`(defs への名前参照)が現れる
(`schema/mod.rs:896-903`)。これは μ 型を名前付き定義の組で表す、
iso-recursive(展開を明示的な操作として扱う)流儀に相当する。
展開(unfold)に当たるのが `shape_of` / `shape_with_defs` で、
トップレベルの Ref 連鎖を defs に対して剥がし、`Shape` ビューを返す。

ここで型理論的に面白いのは、展開済みかどうかを Rust の型で区別している
点である。

- `Shape`(`mod.rs:578-629`)には Ref 変種が存在しない。
  検証・差分・LSP などの「型の意味を消費する」側はこのビューしか
  受け取れないため、未展開の Ref に対して照合してしまう状態が
  そもそも書けない。
- `RawShape`(`mod.rs:752-809`)は Ref を保ったまま投影する。
  ワイヤ転送や JSON 往復のような「構造だけを運ぶ」用途専用で、
  循環スキーマ上で無限展開に陥らない。
- `resolve_refs_with_defs` の戻り値 `ResolvedAttrType` はコンストラクタが
  モジュール私有で、「Ref でないことが証明済みの型」をリゾルバだけが
  作れる。

つまり「fold された型」と「unfold された型」を別の Rust 型にすることで、
equi-recursive 的な暗黙の同一視(と、それに伴う無限再帰の危険)を排除
している。carina#3349 / #3371 の一連の作業で `shape(empty_defs())` の
ような誤用が型レベルで書けなくなったのは、この iso-recursive 化の完成形
である。なお展開には 256 ホップの上限があり、悪意ある・壊れた defs に
対する停止性は型でなく実行時ガードで担保している。

## 5. 公称性と構造性 — TypeIdentity の部分的部分型付け

列挙やカスタム文字列型には `TypeIdentity { provider: Option<String>,
segments: Vec<String>, kind: String }` という公称的な識別が付く
(`schema/type_identity.rs`)。注目すべきはその等価性・代入可能性の定義で、
単純な全フィールド一致ではない。

- `same_type` は「両者が値を持っている軸だけ」を比較する。
  `provider: None` の型は provider 軸について「より広い型」として振る舞う。
- `assignable_to(sink)` は方向付きで、受け側(sink)が特定している軸は
  送り側も一致しなければならないが、受け側が特定していない軸は問わない。

これは公称型に幅部分型付け(width subtyping)を一軸ずつ載せた構造で、
`aws.iam.Role.Arn <: Arn`(provider 非依存の Arn)のような包摂が成り立つ。
provider 無指定の bare identity は実質的にその kind の上限(top)として
機能する。一方で `aws.Region` と `gcp.Region` のように両軸が特定されて
いれば別型であり、provider 越しの混同は型として弾かれる。
「公称的な核(kind)+ 省略可能な公称軸による包摂」というこの設計は、
Java 的な完全公称型と TypeScript 的な完全構造型の中間にあり、
マルチプロバイダ環境での型再利用(共通の Arn 型)とプロバイダ間の
混同防止を一つの機構で両立させている。

## 6. 漸進的型付けとしての Unknown / Deferred 値

値の表現(`resource/mod.rs:609-666`)は最上位で
`Value = Concrete(ConcreteValue) | Deferred(DeferredValue)` に分かれ、
Deferred 側には参照・補間・関数呼び出しと並んで
`Unknown(UnknownReason)` がある。これは Terraform の
「(known after apply)」に相当する、apply してみるまで値が決まらない
ことの一級表現である。

型理論的にはこれは漸進的型付け(gradual typing)の動的型 `?` に近い。
特徴的なのは二点:

1. 検証は Unknown を素通しする(`validate_type_expr_value` 冒頭の
   skip arm)。「上流の apply で解決されるはずの値」に対して偽の型エラーを
   出さない、楽観的な整合性(gradual typing でいう consistency 関係)を
   採っている。
2. 等価性において Unknown は何とも等しくない(カスタム `PartialEq`)。
   IEEE 754 の NaN と同じ非反射的な扱いで、三値論理の「不明」を
   二値の等価判定に埋め込む際の保守的な選択である。差分計算では
   「不明な値は変化したとみなす」方向に倒れるため、健全(偽陰性を
   出さない)だが完全ではない(偽陽性の diff は出うる)。

`UnknownReason` が 7 変種の列挙で「なぜ不明か」(上流参照、for 変数、
編集途中の補間など)を保持しているのは実務的に優れた点で、単なる ⊥ では
なく出所付きの ⊥ になっており、診断品質を支えている。

## 7. typestate と相型(phase typing)— メタ言語側の主役

Carina の型安全性投資が最も厚いのがここで、「処理が進むと値の型が変わる」
typestate パターンが系統的に使われている。代表例:

| raw 側 | resolved 側 | 生成者 | 防いでいる事故 |
| --- | --- | --- | --- |
| `RawEnumIdentifier` | `CanonicalEnumValue` | `EnumValueResolver` のみ | 未解決の DSL 綴りと API 値の直接比較(carina#3438) |
| `AttributeType`(Ref 含む) | `ResolvedAttrType` / `Shape` | `resolve_refs_with_defs` / `shape_of` のみ | 未展開 Ref への照合・無限再帰(#3340/#3349) |
| `ResourceName::Pending` | `ResourceName::Bound` | 名前抽出処理 | 「名前未確定」と「空文字列」の混同 |
| `EphemeralId` | `PersistentId` | 相互変換なし | 合成ノードの ID で状態を引く誤り(#3293) |
| `Effect`(8 変種) | `BasicEffect`(3 変種) | `Effect::as_basic()` のみ | 基本実行器に Wait/Move 等が流れ込む(#3164) |
| `File<ParsedExportParam>` | `File<InferredExportParam>` | `map_export_params` | 推論前後のフェーズ混同 |

`File<E>` のフェーズパラメータは添字付き型(indexed type)の簡易形で、
パース直後と型推論後を別の Rust 型にしている。`CanonicalEnumValue` は
コンストラクタ私有 + リゾルバ専有という典型的な「証明を持ち運ぶ値」
(リゾルバを通った事実の証人)であり、依存型のない言語で
事後条件を型に焼き込む標準手法である。

この一覧が示すのは、CLAUDE.md の「壊れた状態を表現不能にする」という
規範が散発的な工夫ではなく、raw/resolved の対を作る→生成者を一箇所に
絞る→消費者の引数型を resolved 側に変える、という反復可能な手順として
運用されていることである。

## 8. 存在型としての opaque AttributeType

`AttributeType` は `pub struct AttributeType { pub(crate) kind: AttrTypeKind }`
という不透明 newtype で、変種列挙 `AttrTypeKind` は crate 私有である。
下流クレートは

- 生成: `string()` / `enum_()` / `struct_()` / `ref_()` などの
  スマートコンストラクタ経由のみ
- 観測: `shape_of`(Ref 剥がし済み)か `raw_shape`(構造転送用)の
  二つの射影経由のみ

という API に閉じ込められる。これは存在型による抽象データ型
(∃t. インターフェース)の Rust 流の実装で、表現(変種の集合)を隠す
ことで「下流が `Ref` に直接パターンマッチして剥がし忘れる」クラスの
バグを構文的に不可能にしている。観測を一つの関数群に集約したことで、
表現変更(変種追加)の影響範囲も carina-core 内に閉じる。
`_ =>` ワイルドカードで `Ref` を握り潰す事故(carina#3340 系の懸念)への
最終的な防壁もこの不透明化である。

## 9. 型主導の等価性 — 等価判定は型の関数である

差分エンジンの中核 `type_aware_equal`(`differ/comparison.rs:53`)は、
等価性が型ごとに定義される(type-directed equality)ことを明示した
設計である。同じ値の組でも、期待される型によって判定が変わる:

- `List { ordered: true }` は列として位置比較、`ordered: false` は
  多重集合として比較
- `Enum` は API 値のテキストを大文字小文字無視で比較しつつ、
  `CanonicalEnum` 同士なら identity(公称軸)も厳密に比較
- `Union` はメンバを順に試す(合併の等価性はメンバ等価性の存在量化)
- `Secret` はハッシュ(argon2)経由の比較で、平文を状態に残さない

正規化 `canonicalize_with_type`(`value.rs:1442`)も同じ思想で、
`Union[String, List<String>]` → `StringList` の標準形化、
`EnumIdentifier` → `CanonicalEnum` の解決などを型に導かれて行う。
型理論の言葉では、値の同値関係を定義的等価(definitional equality)
として型ごとに与え、比較の前に正規形(canonical form)へ簡約してから
判定する、という型付き等価性の標準的な構成になっている。
過去の不具合(state の列挙値が永遠に `~` diff を出し続ける現象)は
正規化を経ない比較経路が残っていたことが原因で、これは
「正規形を経ない等価判定は不健全」という理論側の予言どおりの事故だった。

## 10. 効果の具体化 — Effect as values

`Effect`(`effect.rs:82`)は Create / Update / Replace / Delete / Import /
Remove / Move / Wait の 8 変種を持つ純粋な値であり、`Plan` はその列に
すぎない。実行は `Provider` トレイト(解釈器)が後から与える。

これは副作用の具体化(reification)であり、構造としては free monad /
algebraic effects の初等形 — 「プログラム = 効果の自由構造、意味 =
解釈器」という分離 — に対応する。継続を持たない(効果の結果に依存して
次の効果を構成する部分は plan 生成側で済ませてある)ため、モナドという
より効果の自由モノイド(列)+ 複数解釈器(実プロバイダ、mock、表示)
と言うのが正確である。この分離が「plan は検査可能・表示可能・保存可能、
apply だけが世界に触る」という Carina の中心的性質を支えている。
`BasicEffect` への絞り込み(§7)は、解釈器ごとに受理できる効果の
部分集合を型で表した、効果の行(effect row)の制限の素朴な実装と
読める。

## 11. 型消去境界 — WIT/WASM プロトコル

WASM プラグイン境界(carina-provider-protocol / wasm_convert.rs)では
型情報の一部が不可逆に消える。

- `EnumIdentifier` / `CanonicalEnum` → `StrVal`(タグと identity が消える)
- `List` / `Map` → JSON 文字列(WIT が再帰型を持てないため)。
  往復後に `StringList` と `List` の区別は復元されない
- `ResourceId.provider_instance`(名前付きプロバイダルーティング)は
  WIT レコードに存在せず、戻りは常に `None`
- 検証クロージャは渡れない(§3 の `CustomTypeLookup` で代替)

型理論的には、これは静的型付き言語のコンパイルにおける型消去
(type erasure)と同型の現象である。境界の向こうは実質的に
単型(uni-typed)の世界で、戻ってきた値はホスト側がスキーマを使って
再び型を着せ直す(canonicalize / lift)。事故のクラスもこの構図から
予測可能で、「消去された情報に依存する処理が境界の向こう・戻り側に
ある」場合に静かに壊れる(#3364 の pattern 脱落、awscc#251 の
String→EnumIdentifier lift など、いずれもこのクラス)。ワイヤに
protocol_version を載せて非互換を検出する対策は、消去境界に
バージョン付き契約を置く標準的な緩和策である。

## 12. 評価

### 強み

1. 「壊れた状態を表現不能にする」が手順化されている。raw/resolved 対 +
   生成者の一意化 + 射影 API の集約という同じ形が、列挙値・Ref・ID・
   効果・パーサフェーズに反復適用されており、個々の工夫ではなく
   設計言語になっている。
2. 再帰型の扱いが理論的に正しい。iso-recursive 化(Shape に Ref が
   ない)は、equi-recursive 的な暗黙展開で無限再帰した過去の事故への
   根治であり、新しい消費者が現れても剥がし忘れが起きない。
3. 等価性・正規化が型主導で一元化されている。差分という Carina の
   中核機能が「正規形に簡約してから型ごとの同値関係で比較」という
   原則に載っている。
4. Unknown の意味論が一貫して保守的。検証は素通し、等価性は不成立、
   という組み合わせは「偽の型エラーを出さず、怪しい diff は出す」
   方向に揃っており、インフラツールとして健全側に倒れている。
5. 公称軸の部分的部分型付け(TypeIdentity)が、共通型の再利用と
   プロバイダ間混同防止を一つの機構で両立している。

### 弱み・理論的な負債

1. DSL 値の型検査が全面的に実行時である。`TypeExpr` の整合性も
   値の検証も plan/validate 時の動的検査で、DSL それ自体の静的型付けは
   存在しない。LSP 診断が事実上の静的検査として機能しているが、
   検証ロジックの二重実装(validate と LSP のパリティ問題)という
   形でコストが顕在化している。
2. タグなし Union の判別はヒューリスティック。スコアリングによる
   メンバ選択は重なりの大きい合併で原理的に曖昧であり、判別子付き
   合併(tagged union)を DSL に持たない限り根本解消はしない。
3. デフォルト値・computed(read-only)属性が型に乗っていない。
   `deferred_populate` と `ExplicitFields` で運用的に補っているが、
   「この属性は誰が書くのか(ユーザー / プロバイダ / サーバ)」という
   出所が型でなくフラグと投影で表現されており、差分の「見かけの削除」
   問題のような症状が出るたびに個別対処になりやすい。
4. WIT 境界の消去が広い。`StringList`/`List` の区別喪失や
   provider_instance の脱落は「境界の向こうで型を再構成する」コードに
   暗黙の前提として漏れ出す。再帰型を JSON 文字列で運ぶ現状は、
   WIT の表現力の制約による妥協であり、境界をまたぐ型保存性は
   プロトコル設計の継続課題である。
5. 篩の不透明部分(検証クロージャ)は合成も直列化も比較もできない。
   pattern/length のような一階表現への寄せが進むほど境界事故の
   クラスは縮むが、現状は二系統(構造的制約とクロージャ)の併存で、
   どちらで検証されるかが型から読めない。

### 型理論的に見た発展方向(参考)

- Union への判別子導入(または DSL 構文での tagged union)。
- 属性の出所(ユーザー指定 / プロバイダ算出 / サーバ既定)を
  `AttrTypeKind` 上のモダリティとして型に昇格させると、
  `ExplicitFields` 投影が型主導の操作になる。
- WIT 境界の値表現を JSON 文字列から構造化(WIT の resource /
  再帰の将来サポート、あるいは自前のタグ付きツリー)へ移すと、
  §11 の消去クラスの事故が構文的に縮む。

## 13. まとめ

Carina の型システムは、対象言語としては「篩型と公称/構造ハイブリッドの
識別を備えた、実行時検査ベースの漸進的型付き DSL」であり、メタ言語と
しては「typestate・存在型・iso-recursive な再帰型管理を系統的に使った
Rust 実装」である。理論的に最も特徴的なのは、動的な対象言語の健全性を
静的なメタ言語の型で底支えするという役割分担が明確なことで、
過去の重大バグ(無限再帰、未解決値の比較、効果の誤ルーティング)の
修正がいずれも「実行時ガードの追加」ではなく「その状態を表現不能にする
型の導入」として決着している点に、設計思想の一貫性が表れている。
