# Apply の partial-update を state に保存する (awscc#371 / carina#3571)

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。

本設計は awscc#371 を契機とするが、解決の柱は carina-core の
`Provider::update` 境界に置く。実装は carina#3571 + 後続の provider
PR の 4 リポジトリにまたがるチェーンになる。

`notes/specs/2026-06-15-partial-create-outcome-design.md` で固めた
create 側の設計と対称な形を取る。同じ柱、同じ型シェイプ、同じ
storage / plan-input 分離。本ドキュメントは差分だけを述べる。

## 課題

create 側で固定したのは「リソース作成は成功したが post-create read
が失敗」を表現できる型シェイプだった。update 側にも同じクラスの
問題がある。

`Provider::update` の戻り型は現在 `ProviderResult<State>` の二値で、
「クラウド側はミューテーションを受け取ったが provider がその完了を
完全に観測できなかった」を表現できない。

具体例は awscc#371 が記録している。awscc の `cc_update_resource` は
CloudControl の `wait_for_operation_with_attempts` を呼び、戻り値の
`WaitOutcome::PartialOrFailed { identifier, status_message }` を
`wait_for_operation` シム経由で
`Err(ProviderError::api_error("Operation failed: {status_message}"))`
に畳んで返す。結果、ミューテーションは AWS 側に適用済みなのに、
carina から見ると update は単純失敗で state は pre-update の値の
まま据え置かれる。次の plan は同じ update を再提案し、リソース型に
よって 400 / 409 / no-op を引き起こす。

create 側の awscc#369 と全く同じ「sibling code path で同じ罠」だが、
sibling は別の operation (update) なので、create と同形の型シェイプ
拡張で受けるのが筋。

## 解決の柱

create 側の設計と一致する三本柱を update にも当てる。

### 柱 1 — Provider trait と WIT の境界

`Provider::update` の戻り型を `ProviderResult<UpdateOutcome>` に
変更する。`UpdateOutcome` は carina-core の新 enum:

```rust
pub enum UpdateOutcome {
    Success { state: State },
    PartialSuccess {
        state: State,
        diagnostic: PartialReadDiagnostic,
    },
}
```

`PartialReadDiagnostic` は本 PR で `PartialCreateDiagnostic` を改名
した型である。意味論は「provider はミューテーションを完了させたが、
それに続く observation が失敗した」で、create と update の双方で
同一。フィールドも完全に同じ:

```rust
pub struct PartialReadDiagnostic {
    reason: String,
    missing_attributes: Vec<String>,
}
```

`PartialCreateDiagnostic` を残して別名にする案は採らない。CLAUDE.md
の「new caller tomorrow」テストに照らすと、create 用と update 用の
二つの diagnostic 型がある状態は将来「どっちを使うべきか」の判断を
新 caller に投げる convention になる。型は一つに統合する。

`CreateOutcome` 側のフィールド型は `PartialCreateDiagnostic` から
`PartialReadDiagnostic` に置換する。`CreateOutcome::partial_success`
constructor の引数 / 戻り値の意味論は不変。

WIT 側 (`carina-plugin-wit/wit/types.wit` と `provider.wit`) も同形に
拡張する。既存の `create-outcome` / `create-partial-success` の
`diagnostic` フィールドの型名を `partial-create-diagnostic` →
`partial-read-diagnostic` にリネームし、`update-outcome` /
`update-partial-success` を新規追加する。

```wit
variant update-outcome {
    success(state),
    partial-success(update-partial-success),
}

record update-partial-success {
    state: state,
    diagnostic: partial-read-diagnostic,
}

record partial-read-diagnostic {
    reason: string,
    missing-attributes: list<string>,
}
```

`provider.wit` の `update` を:

```wit
update: func(
    id: resource-id,
    identifier: string,
    request: update-request,
) -> result<update-outcome, provider-error>;
```

create と update は同じ `PartialReadDiagnostic` を共有するため、
新 record (`create-partial-success` の diagnostic field) と既存
record の整合を保つ。

### 柱 2 — provider 側判定の構造的シグナル

awscc 側の判定は create とほぼ同形になる。`cc_update_resource` の
中で `wait_for_operation_with_attempts` の戻り値を:

```text
WaitOutcome::Success { identifier }
  → 通常の post-update read で hydrate → UpdateOutcome::Success

WaitOutcome::PartialOrFailed { identifier, status_message }
  → identifier を使って read_resource を再試行
     ├── 成功 → desired carry-forward の上で UpdateOutcome::Success
     └── 失敗 → UpdateOutcome::partial_success(state, reason, missing)
               state は from の identifier を保持しつつ
               attribute は request.to (patch 適用後) から carry forward
```

HandlerErrorCode のホワイトリストは持たない。`ProgressEvent.identifier`
が `Some(non-empty)` で `OperationStatus = Failed | CancelComplete` の
ときが「ハンドラはミューテーションを完了させた」を意味する単一の
構造的シグナル。create 側と同じ規律。

partial state の `attributes` の中身は create 側と異なる。create では
リソースが新規なので desired を識別子のみの state に carry forward
した:

```text
state = State::existing(id, HashMap::new()).with_identifier(identifier)
```

update では既存リソースに対して patch を適用したので、carina-core
側で `apply_patch_to_state(from, patch)` を計算した結果が provider 側
で見えている (現状の awscc 実装で `from` と `to` を扱っている経路)。
これを partial state の `attributes` の出発点にする:

```text
state = State::existing(id, post_patch_attributes).with_identifier(from_identifier)
```

`missing_attributes` には「user が authored したが post-update read で
取り戻せなかった attribute」を入れる (create 側の R2 と同じ filter)。

これによって、partial-update 後の state は「user が指定した値を
中心とした best guess」になり、次の plan は read 失敗した attribute
を Unknown として再表示する。

### 柱 3 — 表示挙動と exit code

`UnknownReason::PostCreateReadIncomplete` をそのまま流用する。
意味論的には post-update read incomplete も「provider が観測でき
なかった」結果で、`render_unknown` の文言は同じ:

```text
(known after next apply: post-create read failed — {detail})
```

文言の「post-create」は厳密には誤読を招くが、本ドキュメントでは
「観測失敗」の意味として包括的に扱う。文言を「post-handler read
failed」のような汎用語に変えるかどうかは別途検討する。

apply の event は新 variant を追加せず、既存の
`ExecutionEvent::EffectPartiallySucceeded` をそのまま使う。
ExecutionState の `partial_count` / `partial_diagnostics` の集計も
変えず、create/update のどちらでも同じ Aggregator に乗る。
`ApplyExitCode::PartialSuccess` (= 2) も同じ。

### 柱 4 — スコープ

実装は 4 PR チェーンに分解する。create 側と同形だが、WIT に
`update-outcome` を追加するため `carina-plugin-wit` の PR が先行する。

1. **`carina-plugin-wit` PR**: `update-outcome` / `update-partial-success`
   variant 追加 + `partial-create-diagnostic` → `partial-read-diagnostic`
   リネーム。
2. **carina-core PR** (closes carina#3571): `UpdateOutcome` enum +
   `PartialReadDiagnostic` (旧 `PartialCreateDiagnostic` のリネーム) +
   `Provider::update` シグネチャ変更 + WIT submodule bump + executor
   の create/update partial 統一 + mock provider partial-update フック +
   e2e test。
3. **`carina-provider-aws` PR**: `Provider::update` 戻り型変更に
   追従するだけの mechanical 変更。
4. **`carina-provider-awscc` PR** (closes awscc#371): `cc_update_resource`
   から `wait_for_operation` シム経由を解除し、`WaitOutcome::PartialOrFailed`
   を `UpdateOutcome::partial_success` に変換する経路を実装。

3 PR の依存は厳密に 1 → 2 → 3 → 4。

### Delete について

`Provider::delete` は本設計の対象外とする。理由:

- delete の成功基準は「リソースの不在」であり、post-delete read で
  「まだ存在する」が観測されたら "delete failed" として扱うのが
  正しい semantics。「ミューテーションは完了したが観測できなかった」
  と「観測したらまだ存在する」の区別は delete では曖昧で、partial
  概念の意味づけが create/update と対称にならない。
- awscc 側で具体的な triggering case が今のところ無い。
- create/update を統一型シェイプにまとめた上で、delete を追加する
  かどうかは新たな triggering case が出てから判断する方が、
  invariant を一回ずつ証明できる。

これは create 側設計文書 (§「将来 update/delete/read にも同様の
partial-outcome が必要になったら」) の方針を踏襲する。

## State の partial_read marker は流用する

`PartialReadMarker` および `State::partial_read` フィールドは create
側 PR1b で導入済み。update でも同じフィールドを使う。

`UpdateOutcome::PartialSuccess` 経由で executor に届いた state は
`UpdateOutcome::into_state_for_writeback()` (新設) によって marker が
stamp される。これは `CreateOutcome::into_state_for_writeback()` と
全く対称な実装で、`PartialReadDiagnostic` を `state.partial_read =
Some(PartialReadMarker { ... })` に詰める。

`restore_partial_read_markers` の挙動は変えない。marker は provenance-free
で、create 由来か update 由来かを記録しない。意味論は「次回 read で
attribute を取り戻すべき」で create でも update でも同じ。

## State file 互換性

state file の version bump は不要。`partial_read` フィールドは create
側で既に追加済み。update 由来の marker も同じシェイプで書かれる。
v7 state file (どちらの marker も None) を読んでも問題ない。

## 検証

実装 PR で以下を満たすことを確認する:

1. **mock provider で partial-update path を再現するテスト**。create
   側の mock partial フックを update にも拡張し、apply / state / 次
   plan が partial-create と同じ shape で動くことを e2e で検証する。
2. **awscc 側のユニットテスト** — `cc_update_resource` の
   `WaitOutcome::PartialOrFailed` 経路で `UpdateOutcome::PartialSuccess`
   が返ることを mock cloudcontrol client で検証する。
3. **`UpdateOutcome` の exhaustive match を要求するコンパイル時テスト**。
   compile-fail doctest を `differ` 周辺に追加する。
4. **regression test** — awscc#371 が記録するシナリオを acceptance test
   側で再現する。create 側と同様、handler の ProgressEvent fixture で
   十分。

## 引用される失敗モード

- carina#3324 / PR #3325 → carina#3326: 「runtime resolver fix at three
  consumer sites passing 5-round review ≠ root-cause」。本設計は同じ教訓
  を update に適用する。
- carina#3567 / awscc#369: create 側で同じ柱を立てた。update が同形
  partial を持たないと、「次の sibling operation tomorrow」が同じ穴を
  再生する。

## 関連

- awscc#371 (本設計の trigger)
- carina#3567 (create 側設計、同形)
- carina#3570 (binding-index に `PlanInputState` を通す follow-up、独立)
- awscc#369 (create 側の concrete trigger、すでに merged)
