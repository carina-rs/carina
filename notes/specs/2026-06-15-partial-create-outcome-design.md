# Apply の partial-create を state に保存する (awscc#369)

## ステータス

設計提案。本ドキュメントは設計側で、実装 PR より先にマージする必要が
ある（CLAUDE.md「Design PR must merge before implementation PR」）。

本設計は awscc#369 を契機とするが、解決の柱は carina-core の
`Provider::create` 境界に置く。実装は carina#（本 PR と同時に
filing する後続 issue） + awscc#369 の 2 リポジトリにまたがる
チェーンになる。本 PR で固めるのは「型シグネチャの形」「provider
側判定の構造」「plan/apply の表示挙動」の三点で、コードは書かない。

## 課題

CloudControl の create handler は内部で複数の AWS API を順に呼ぶ。
典型的には `CreateX` で実体を作り、`DescribeX` 等で active になる
まで poll し、最後に post-create validation の read (handler 内部の
別 API) を行ったうえで `ProgressEvent.OperationStatus = Success` を
返す。最終 read が失敗すると `OperationStatus = Failed` +
`HandlerErrorCode = AccessDenied` 等で終わる。**この時点で AWS 側に
リソース実体は既に存在し続けている。**

awscc#369 で報告された例は ELBv2 LoadBalancer:

```
✗ Create awscc.elasticloadbalancingv2.LoadBalancer alb took 2m 47.2s
  → Failed to get resource: AccessDeniedException: ...
    User: ... is not authorized to perform:
    elasticloadbalancing:DescribeCapacityReservation
    (HandlerErrorCode: AccessDenied, ...)
```

`2m 47.2s` の duration はリソースが active になるまで handler が
待った時間であり、create 自体は完了している。実際 `aws elbv2
describe-load-balancers` を叩くと `State.Code = active` の LB が AWS
に存在する。それでも carina は state に何も書かない。

現状の経路:

* `carina-provider-awscc/src/provider/operations.rs:79-156` の
  `create_resource` が CloudControl `cc_create_resource` →
  `read_resource` を順に呼び、後者が失敗すると関数全体が
  `Err(ProviderError)` を返す。
* `carina-core/src/executor/basic.rs:520-551` の create エフェクト
  実行は、`Err` を受けると `BasicEffectResult::Failure` を作る。
* `process_basic_result`（`basic.rs:413-445`）は Failure を受け取る
  と `exec.applied_states.insert(resource_id, s)` に到達せず、
  state に何も書き込まない。

結果として AWS 側 orphan が残り、次回 plan は新規 create を提案
し、apply は name collision で 409 になる。`carina-rs/infra` の
Publish API ALB stack で実際にこの状態になり、`aws elbv2
delete-load-balancer` で手動清掃するしかなくなった。

CloudControl の create が「途中まで通ったが post-create read で
失敗」したことは、`ProgressEvent.identifier` フィールドに値が
入っているかどうかで構造的に判別できる。AWS CloudControl の仕様上、
handler が `Identifier` をセットしたあとに失敗したケース（=create
自体は完了済み）と、handler 起動前に失敗したケース（=create が
そもそも走っていない）は ProgressEvent 上で区別できる。awscc 側
は現状 `OperationStatus::Success` の分岐でのみ identifier を取り
出し（`cloudcontrol.rs:474-476`）、Failed では捨てている。

### 同じクラスのケース

ELBv2 LoadBalancer の AccessDenied は具体例の一つにすぎない。同じ
クラスの「create 自体は通ったが handler が完了報告できない」現象
は次のエラーコードでも起こりうる:

* `AccessDenied` — handler 内部 read の権限欠け（本 issue 例）
* `Throttling` — post-create describe の rate limit
* `ServiceInternalError` / `GeneralServiceException` — AWS 内部の
  一過性エラー
* `NetworkFailure` — handler 経路の通信障害

これらを HandlerErrorCode のホワイトリストで列挙する設計は脆い
（リスト漏れが新しい orphan を生む）。後述の「Identifier の有無」
を構造的判定の単一基準にする。

## 解決の柱

設計判断は四点ある。三つのレンズ（長期視点・型安全性・根本原因）
で評価して以下のように決めた。

### 配置 — Provider trait と WIT の境界

`Provider::create` の戻り型を `ProviderResult<State>` から
`ProviderResult<CreateOutcome>` に変更する。`CreateOutcome` は
carina-core の新 enum:

```rust
pub enum CreateOutcome {
    /// Create completed and the post-create read was complete.
    Success { state: State },

    /// The resource was created in the cloud (identifier is known and
    /// stable), but the provider could not fully observe its
    /// post-create state. The state must be persisted so the resource
    /// is not orphaned; missing attributes are filled with
    /// `Value::Deferred(DeferredValue::Unknown(UnknownReason::PostCreateReadIncomplete { .. }))`.
    PartialSuccess {
        state: State,
        diagnostic: PartialCreateDiagnostic,
    },
}

pub struct PartialCreateDiagnostic {
    /// Why the post-create read could not complete (operator-facing
    /// string built from the underlying API error). Surfaces in
    /// apply log and the per-attribute Unknown render.
    pub reason: String,

    /// Names of the DSL-level attributes that could not be read.
    /// Used by the apply log to enumerate what will resurface on
    /// the next plan as `~ update` rows.
    pub missing_attributes: Vec<String>,
}
```

WIT 側 (`carina-plugin-wit/wit/provider.wit` と `types.wit`) も
同形に拡張する:

```wit
variant create-outcome {
    success(state),
    partial-success(tuple<state, partial-create-diagnostic>),
}

record partial-create-diagnostic {
    reason: string,
    missing-attributes: list<string>,
}

// provider.wit
create: func(
    id: resource-id,
    request: create-request,
) -> result<create-outcome, provider-error>;
```

**型シグネチャに partial-success が現れるのが本設計の主眼。** 案 ii
（awscc が `Ok(State)` で Unknown を埋めて warning は別チャネル）と
案 iii（`ProviderError` に identifier を持たせ caller が振り分け）
は CLAUDE.md の「new caller tomorrow → 型で答える」テストに落ちる:
新しい provider が partial-create を扱おうとするとき、型が
`Result<State, _>` だと「Ok にして埋める」という convention を
documentation で守るしかない。`CreateOutcome` enum なら新 provider
は exhaustive match で variants を返さざるを得ず、partial を「忘れる」
ことが型レベルで不可能になる。

将来 update/delete/read にも同様の partial-outcome が必要になっ
たら、それぞれ `UpdateOutcome` / `DeleteOutcome` / `ReadOutcome`
を別 enum で導入する。今回の設計では create に絞る（read は失敗時
に state を消す挙動が別 issue、update/delete はそもそも CloudControl
の挙動が異なる）。enum を共通化するのは observational symmetry が
証明されてからで十分。

### 解決方式 — provider 側判定は ProgressEvent.identifier の有無

awscc 側で `OperationStatus::Failed` を受けたとき、次の構造的判定で
振り分ける:

```text
OperationStatus = Failed
├── ProgressEvent.identifier が None
│     → 従来通り Err(ProviderError) を返す
│        （handler 起動前 / Identifier セット前の失敗）
│
└── ProgressEvent.identifier が Some(id)
      ├── id を使って read_resource を再試行
      │
      ├── read 成功 → Ok(CreateOutcome::Success { state })
      │   （handler の HandlerErrorCode は記録のみ、状態は full）
      │
      └── read 失敗 →
            state を identifier + Unknown 埋めの attributes で組み、
            PartialCreateDiagnostic に handler の status_message と
            欠落 attribute 名を入れて、
            Ok(CreateOutcome::PartialSuccess { state, diagnostic })
```

**HandlerErrorCode のホワイトリストは持たない。** AccessDenied /
Throttling / ServiceInternalError 等を分岐に書き並べる設計は脆く、
新コードが増えるたびに orphan が再発する。identifier の有無は
CloudControl 仕様で「handler が Identifier を確定させた後の失敗
かどうか」を一意に表す signal なので、これだけを単一の構造的基準
にする。

provider が assumed-role 経由でも identifier の入手経路は同じ。
awscc が CloudControl から受け取る ProgressEvent は creator role と
直交している。

### 表示挙動 — Unknown reason に新 variant を追加し、plan / apply 双方で説明的に出す

`UnknownReason`（`carina-core/src/resource/mod.rs:801-855`）に新
variant を追加する:

```rust
pub enum UnknownReason {
    // ... 既存 variants ...
    PostCreateReadIncomplete {
        /// Short, user-facing detail. Built by the provider from the
        /// handler's status_message.
        detail: String,
    },
}
```

`render_unknown` (`value.rs:129-147`) は次を返す:

```text
"(known after next apply: post-create read failed — <detail>)"
```

apply 実行中の event stream には新 event を追加する:

```rust
ExecutionEvent::EffectPartiallySucceeded {
    resource: ResourceId,
    identifier: String,
    diagnostic: PartialCreateDiagnostic,
}
```

CLI の apply 表示は次の形を取る:

```text
⚠ Create awscc.elasticloadbalancingv2.LoadBalancer alb (partial)
  → identifier: arn:aws:elasticloadbalancing:...:loadbalancer/app/...
  → post-create read failed: AccessDeniedException ...
  → missing attributes: dns_name, canonical_hosted_zone_id,
    load_balancer_arn, security_groups
  → state recorded; re-run apply to complete the read
```

apply の終了コードは partial があったときは非ゼロにする。新規の
ExitCode variant `PartialSuccess` を導入し、Success / PartialSuccess
/ Failure の三値とする。CI の手元では「成功扱いで通す」のは絶対に
避けたい — partial は operator の注意が要る状態であり、CI のチェッ
クが緑になるべきではない。

次回 plan の表示:

```text
~ update awscc.elasticloadbalancingv2.LoadBalancer alb
    dns_name              = (known after next apply: post-create read failed — AccessDeniedException ...)
    canonical_hosted_zone_id = (known after next apply: post-create read failed — AccessDeniedException ...)
    ...
```

これは既存の `~ update` パスをそのまま使う（state にエントリが
ある + attributes に Unknown がある = 普通の `~ update` 表示）。
plan display 側に新コードは要らない。

### スコープ — 一発で provider 三方とも対応

実装は 3 PR チェーンに分解する:

1. **carina-core PR**: `CreateOutcome` enum + `UnknownReason::PostCreateReadIncomplete`
   variant + `Provider` trait シグネチャ変更 + WIT 拡張 +
   `executor/basic.rs` の partial 分岐 + `ExecutionEvent` 新
   variant + apply display + ExitCode 三値化。mock provider を新
   トレイトに追従させる。
2. **carina-provider-aws PR**: `Provider::create` の戻り型変更に
   追従するだけの mechanical 変更。aws provider は CloudControl を
   使わないので、当面は常に `CreateOutcome::Success` を返す。
3. **carina-provider-awscc PR (= awscc#369 本体)**: 上記「解決方式」
   セクションの判定を `provider/operations.rs` と
   `provider/cloudcontrol.rs` に実装。`wait_for_operation_with_attempts`
   が identifier を Result の Err 側にも乗せて返すように再設計し、
   `create_resource` 側で partial 分岐を扱う。

3 PR の依存は厳密に 1 → 2 → 3。1 がマージされて初めて 2/3 の git
依存 bump が動ける。3 PR 全部がマージされた後、carina-rs/infra で
ELB stack を再 apply して回復確認する（orphan の手動清掃は別
issue で扱う）。

**aws provider と awscc provider の両方を同じ trait 変更に追従さ
せるのが必須。** 片方だけ更新する誘惑（「awscc 固有の問題だから
aws はそのままでいい」）は CLAUDE.md「Cross-repo changes: never
assume the sibling provider is unaffected」に正面衝突する。trait
変更は wire-format 境界（WIT）も動かすので、aws/awscc 両方の rev
bump が要る。

## 対象外

* **orphan の手動清掃自動化**。本設計は「次の create 失敗から
  orphan を発生させない」だけを扱う。既存 orphan の回収（過去に
  本バグで作られたリソースを carina state に取り込む）は別 issue。
  `carina import` サブコマンドの導入はそこで議論する。
* **post-create read の retry/backoff**。awscc 側で read を 2-3
  回 retry してから partial に落とす設計も考えたが、本 PR では入
  れない。理由: AccessDenied は retry しても通らない（permission
  欠けは時間で変わらない）、Throttling は CloudControl 側で既に
  retry されている（handler は AWS API を直接叩いている）。retry
  は別 issue で観測データを取ってから議論する。
* **handler の internal read failure を non-fatal にする CloudControl
  API 側の改善**。これは AWS への feature request の領域で、carina
  側で扱えない。
* **HandlerErrorCode の細分化された UI 表示**。今は status_message
  をそのまま reason に流すだけにとどめる。code 別の hint
  （「AccessDenied なら IAM ポリシーを見直してください」等）は別
  issue で UX 改善として議論する。

## 検証

実装 PR で以下を満たすことを CI で確認する:

1. **mock provider で partial-success path を再現するテスト**。
   `carina-provider-mock` に「次の create は partial を返す」と
   設定できるフックを足し、CLI executor が `applied_states` に
   state を入れること・次回 plan が `~ update` を出すこと・apply
   の exit code が PartialSuccess になることを e2e で検証する。
2. **awscc 側のユニットテスト** — `wait_for_operation_with_attempts`
   に「OperationStatus = Failed + Identifier = Some(...)」の
   ProgressEvent を食わせて、関数が partial path に分岐すること
   を mock cloudcontrol client で検証する。
3. **`CreateOutcome` の exhaustive match を要求するコンパイル時
   テスト**。`UnknownReason::PostCreateReadIncomplete` の追加と
   同様、`#[non_exhaustive]` 属性は付けない（CLAUDE.md「don't
   reach for type-shape-weakening tools」）。
4. **regression test** — awscc#369 の元のシナリオを再現する
   acceptance test を `carina-provider-awscc/acceptance-tests/`
   に追加する。SimulatePrincipal を使う必要はなく、handler の
   ProgressEvent fixture で十分。

5-round review と verify は本設計を実装する各 PR で別途回す。

## 引用される失敗モード

設計判断の出発点となった既往の失敗:

* **carina#3324 / PR #3325 → carina#3326**: 「runtime resolver fix
  at three consumer sites passing 5-round review ≠ root-cause」。
  本設計はこの教訓を直接適用している — `Result<State, _>` に
  identifier を持たせる案 (iii) や `Ok(State)` に Unknown を埋め
  る案 (ii) は両方ともこの「型は据え置いて運用で守る」アンチパター
  ンに該当する。
* **carina#3364 / awscc#346** の WIT 境界ミスマッチ — provider trait
  変更が WIT 変更を必ず伴うこと、そして provider 側の rev bump を
  忘れると silent degradation すること。本設計の実装チェーンは
  1 → 2 → 3 の順を厳守し、2/3 が 1 の merge 後にしか進めないよう
  github 上の依存を明示する。

## 関連

* awscc#369（本設計の trigger）
* carina#3496（IAM preflight、awscc#369 と同じ「事前検知できれば
  orphan を防げた」ファミリー。preflight は orphan を作る前に
  止める方向、本設計は作った後の orphan 化を防ぐ方向で、独立に
  価値がある）
* carina#3541（CFn schema の `handlers.create.permissions` 不完全
  問題。本設計は orphan の発生を防ぐが、permissions list が完全
  でも post-create read 失敗は他要因で起こりうるので、解決対象
  は重なるが orthogonal）
