# Stateless Mode Rejection Record

Tracking: carina-rs/carina#4043

## Decision

Do not add stateless mode under the provider-wide capability rule.

The rule for this issue is that a provider is wholly stateful or wholly
stateless. A stateless provider must be able to discover, observe, reconcile,
recover, and delete every resource type it ships without persisted state. A
provider cannot mix stateless and stateful types, and a type cannot opt in by
itself.

The GitHub survey does not meet that bar. Of the 88 resource types documented
by `integrations/terraform-provider-github`, 60 have a plausible natural or
parent-singleton address and 28 require a server-assigned ID, an unsafe content
match, or one-shot operation semantics. Ten of the 88 have at least one
write-only or masked managed input; that count overlaps the address classes.
The requested survey dimensions therefore count as:

| Survey dimension | Count |
| --- | ---: |
| (a) Natural-keyed or parent singleton | 60 |
| (b) Server-ID, unsafe-content-match, or action-only | 28 |
| (c) At least one write-only or masked managed input | 10 |

Using the mutually exclusive reconciliation buckets in this document, the
result is:

| Bucket | Meaning | Count |
| --- | --- | ---: |
| A | Safe stateless address and readable managed state | 51 |
| B | Natural address, but only presence detection plus explicit forced resend can manage the secret value | 8 |
| C | No safe total stateless reconciliation contract | 29 |

The C bucket contains common resources, not obscure edge cases: organization
and repository webhooks, deploy keys, runner groups, repository and
organization rulesets, custom roles, milestones, issues, classic and v2
project objects, environment deployment policies, user keys, and one-shot
invitation acceptance. Matching those objects by a mutable content field would
invent uniqueness that GitHub does not promise. Requiring users to write the
server ID into configuration permits import-like management of an existing
object, but cannot complete create because the desired configuration does not
know the newly assigned ID.

The current GitHub provider is only a scaffold. It plans
`github.actions.OrganizationSecret` and `OrganizationVariable`; repository,
team, and branch-protection resources are explicit non-goals for its initial
release (`carina-rs/carina-provider-github:docs/design.md:6-14`,
`carina-rs/carina-provider-github:docs/design.md:26-46`). The secret is already
the weaker B case: its plaintext is write-only and the planned state-backed
design stores a hash (`carina-rs/carina-provider-github:docs/design.md:65-86`).
Growing that provider into a useful, comprehensive GitHub provider necessarily
introduces C types.

AWS and AWSCC remain stateful providers. This decision removes the earlier
idea that selected AWS types could opt in one at a time. A separate provider
could be designed around a deliberately smaller all-stateless resource set,
but that is a different provider and does not answer whether the GitHub
provider can manage every GitHub resource type statelessly.

## Problem and Acceptance Bar

Carina currently joins a managed resource to its remote object through the
persisted `AppliedIdentifier`. `Provider::read` requires that identifier, while
data-source reads receive resolved input (`carina-core/src/provider.rs:3540-3614`).
The WIT interface likewise says that a resource without saved state does not
cross the managed `read` boundary (`carina-plugin-wit/wit/provider.wit:52-73`).

State carries substantially more than the identifier. `StateFile` persists
lineage, serial, resource instances, and exports
(`carina-state/src/state/mod.rs:484-502`). Its format records authored-field
provenance, mutually exclusive applied versus in-flight current instances,
top-level export compatibility, and nested write-only provenance
(`carina-state/src/state/mod.rs:741-778`). Removing state therefore needs an
answer for identity, partial ownership, secrets, interrupted creates, exports,
saved-plan freshness, locking, and provider-populated outputs.

This issue imposes a stronger first gate: those answers must work for every
managed type in a provider. The following conditions are required before the
other state responsibilities matter.

1. Fully resolved desired input must produce a remote address that the GitHub
   API itself treats as unique in a stated scope, or identify a singleton
   setting under such a parent.
2. Lookup must distinguish found, absent, and unreadable. An authorization
   failure must never be treated as absence.
3. Every managed field must either be observable or have a typed, explicit
   compromise whose plan semantics are honest.
4. Create must be recoverable after an interrupted response. Retrying must not
   create a second object.
5. Update and delete must be able to recover the remote mutation identifier
   from desired input on every run.
6. Renaming a key must either be addressable explicitly or be rejected; it
   must not silently create a second object and abandon the first.

Import support is evidence about an address, not proof of these conditions. A
Terraform importer may list all objects and match a field, or may require the
operator to supply a server ID that only state can remember after create.

### Provider-wide capability and typed enforcement

If this decision is reconsidered, capability belongs to the provider schema
set, not `ResourceSchema`. Conceptually, the SDK would expose disjoint
constructors:

```text
ProviderSchemaSet::stateful(Vec<StatefulResourceSchema>)
ProviderSchemaSet::stateless(Vec<StatelessResourceSchema>)

StatelessResourceSchema::new(
    ResourceSchema,
    NaturalKeyExtractor,
    ObservationContract,
    MutationRecoveryContract,
)
```

There is no per-type `Unsupported` case and no optional stateless field on an
ordinary resource schema. Once a provider selects the stateless constructor,
the only element type accepted by its resource collection requires a natural
key extractor and the observation and recovery contracts. Rust providers
using the SDK therefore cannot build an all-stateless schema set while
registering a plain resource schema. At the plugin trust boundary, decoding
must validate the whole tagged schema set and reject a malformed provider at
load time.

Validation then checks the provider used by the configuration against the
backend mode. `backend none {}` may use only stateless provider schema sets;
an ordinary backend may use only stateful sets. The shared validation path
must feed both CLI diagnostics and LSP diagnostics. This is one provider-level
error, not 88 runtime warnings.

This shape follows the project's root-cause and typestate rules
(`CLAUDE.md:11-81`). It also preserves the identity rule
(`CLAUDE.md:83-135`): scheduler, dependency, plan, diff, and state maps remain
keyed by `ResolvedResourceId`. A `NaturalKey` would be only the typed remote
address. It could be constructed only by the provider-owned extractor from a
fully resolved `ProviderReadyResource`, would own normalization, and would
have no `From<ResourceName>`, `From<Value>`, or cross-type `PartialEq`.
Pending, unknown, empty, and arbitrary user labels therefore cannot enter an
equality-bearing internal key. The survey rejects content matching precisely
because it would violate this boundary.

## Survey Method

The baseline is every Markdown resource document in the checked-out Terraform
GitHub provider snapshot. Each row was checked against its corresponding
`integrations/terraform-provider-github:docs/resources/<type>.md` and, where
present, `integrations/terraform-provider-github:github/resource_github_<type>.go`.
The import parser and CRUD calls show whether the implementation can use a
natural address or retains a server ID.

The API cross-check used
`github/rest-api-description:descriptions/api.github.com/api.github.com.json`.
The table names the relevant REST path or operation ID for the important
distinctions. Path components such as `{hook_id}`, `{key_id}`, `{ruleset_id}`,
`{milestone_number}`, and `{runner_group_id}` are materially different from
`{label_name}`, `{environment_name}`, or `{team_slug}`.

The corpus size was obtained with:

```sh
TF_PROVIDER=${TF_PROVIDER:?set this to an integrations/terraform-provider-github checkout}
find "$TF_PROVIDER/docs/resources" -maxdepth 1 -type f -name '*.md' -print |
  sed 's#.*/##; s/\.md$//' |
  sort |
  wc -l
# 88
```

The OpenAPI paths were inspected with commands of this form:

```sh
GITHUB_OPENAPI=${GITHUB_OPENAPI:?set this to api.github.com.json}
jq -r '
  .paths
  | to_entries[]
  | select(.key | test("hooks|keys|rulesets|milestones|runner-groups"))
  | .key as $path
  | .value
  | to_entries[]
  | select(.key == "get" or .key == "post" or .key == "put" or
           .key == "patch" or .key == "delete")
  | [$path, .key, .value.operationId] | @tsv
' "$GITHUB_OPENAPI"
```

Live responses were observed on `carina-rs` on 2026-10-28 with a read-only
token. They are supporting observations, not a claim about all authentication
modes:

- repository Actions secrets returned only `name`, `created_at`, and
  `updated_at`;
- organization Actions secrets and variables returned HTTP 403 with an
  explicit permission message;
- listing organization webhooks without `admin:org_hook` returned HTTP 404
  `Not Found`, not 403;
- labels, branch protection, environments, Pages, and repository ruleset
  summaries were readable; ruleset summaries contained the numeric `id` used
  by their API URL.

### Classification

`NK` means an API-enforced scoped natural key or a singleton under a resolved
parent. `ID` means that the mutation API addresses the object only by a
server-assigned ID and the evidence did not establish a safe desired-input
lookup. `Action` means the Terraform resource is an operation rather than
durable declarative state. `R` means the managed state can be read; `W` means
at least one managed input is masked, write-only, or unavailable on later
reads.

The final column is the reconciliation verdict:

- A: safe enough for an all-stateless contract, subject to ordinary typed
  permission and rename rules;
- B: addressable, but secret equality is unknowable, so only presence plus an
  explicit forced resend is honest;
- C: content matching, manual IDs, or operation semantics cannot implement a
  total stateless managed-resource contract.

## GitHub Resource Survey

The 88 `TF` rows are the counted baseline. `REST` rows are additional durable
configuration families that are absent from that Terraform list but obvious
from create/update/delete or singleton-setting endpoints in the OpenAPI
snapshot. A REST row may represent multiple scopes when the identity and
failure mode are identical. Transient workflow runs, artifacts, checks,
deployments, comments, notifications, generated tokens, and audit records are
outside the provider-resource boundary; including those one-shot APIs would
only add more C cases.

| Area | Type | API address | Readability | Rename or key change | Stateless treatment | Bucket | Source |
| --- | --- | --- | --- | --- | --- | :---: | :---: |
| Repository | `repository` | NK owner + repository | R | GitHub redirects an old repository name, but desired new name does not reveal the old name | Direct live lookup; rename needs an explicit old-key alias, otherwise new plus unmanaged old | A | TF |
| Repository | `repository_collaborator` | NK owner + repository + login | R | Login or repository key change is a different association | Direct membership lookup; explicit old association removal | A | TF |
| Repository | `repository_collaborators` | Singleton owner + repository | R | Parent rename changes the address | Read and reconcile the complete set only when explicitly authoritative | A | TF |
| Repository | `repository_custom_property` | NK organization + repository + property name | R | Property rename changes the key | Direct scoped lookup; parent property must already resolve | A | TF |
| Repository | `repository_dependabot_security_updates` | Singleton owner + repository | R | None | Read enablement from the parent endpoint | A | TF |
| Repository | `repository_pages` | Singleton owner + repository | R | None | Parent-keyed settings; absence is the missing Pages site | A | TF |
| Repository | `repository_topics` | Singleton owner + repository | R | None | Read and replace the explicit topic set | A | TF |
| Repository | `repository_vulnerability_alerts` | Singleton owner + repository | R | None | Parent-keyed boolean capability | A | TF |
| Repository | `repository_autolink_reference` | ID owner + repository + autolink_id | R | Prefix is immutable in REST | Listing and matching `key_prefix` is possible, but the API does not promise uniqueness; unsafe content match | C | TF |
| Repository | `repository_file` | NK owner + repository + branch + path | R, but commit metadata is operation history | Path or branch change abandons the old path | File content can be compared, but commit author, email, and message are write instructions, not durable fields; not a total resource contract | C | TF |
| Repository | REST-only `RepositoryImmutableReleasesSettings` | Singleton owner + repository | R | None | Direct `repos/enable-immutable-releases` and disable operation | A | REST |
| Repository | REST-only `RepositoryCodeScanningDefaultSetup` | Singleton owner + repository | R | None | Direct `code-scanning/update-default-setup` | A | REST |
| Repository | REST-only `RepositoryPrivateVulnerabilityReporting` | Singleton owner + repository | R | None | Direct enable/disable endpoint | A | REST |
| Repository | REST-only `RepositoryInteractionLimits` | Singleton owner + repository | R | None | Direct set/remove endpoint | A | REST |
| Branches and rules | `branch` | NK owner + repository + branch | R | Rename requires the old branch name | Direct ref lookup; a rename needs explicit old and new keys | A | TF |
| Branches and rules | `branch_default` | Singleton owner + repository | R | May invoke a branch rename, which still needs the old name | Parent-keyed default setting | A | TF |
| Branches and rules | `branch_protection` | NK repository + pattern through importer/list lookup | R | Pattern change changes the key | Resolve the rule by repository and pattern; preserve whole-object fields | A | TF |
| Branches and rules | `branch_protection_v3` | NK owner + repository + branch | R | Branch rename changes the key | Direct `/branches/{branch}/protection` lookup | A | TF |
| Branches and rules | `organization_ruleset` | ID organization + ruleset_id | R | Name is mutable under the retained ID | OpenAPI mutations use `{ruleset_id}`; matching name is a gh-infra convention, not an official REST uniqueness statement | C | TF |
| Branches and rules | `repository_ruleset` | ID owner + repository + ruleset_id | R | Name is mutable under the retained ID | Same ID-only problem; list-by-name is unsafe without a tested provider guarantee | C | TF |
| Actions | `actions_environment_secret` | NK owner + repository + environment + secret name | W plaintext/encrypted value | Rename is delete/create | Presence-only; explicit force resends all selected values | B | TF |
| Actions | `actions_environment_variable` | NK owner + repository + environment + variable name | R | Name change changes the key | Direct scoped lookup; explicit old-key removal | A | TF |
| Actions | `actions_hosted_runner` | ID hosted_runner_id | R | Display settings remain under ID | No provider-enforced desired key; manual ID cannot recover create | C | TF |
| Actions | `actions_organization_oidc_subject_claim_customization_template` | Singleton organization | R | None | Direct parent setting | A | TF |
| Actions | `actions_organization_permissions` | Singleton organization | R | None | Direct parent setting plus paged selected repositories | A | TF |
| Actions | `actions_organization_secret` | NK organization + secret name | W plaintext/encrypted value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Actions | `actions_organization_secret_repositories` | Singleton organization + secret name | R | Secret rename changes the parent key | Read/set selected repository IDs after resolving repositories | A | TF |
| Actions | `actions_organization_secret_repository` | NK organization + secret name + repository ID | R | Parent key change changes association | Resolve repository ID at run time; direct association lookup | A | TF |
| Actions | `actions_organization_variable` | NK organization + variable name | R | Name change changes the key | Direct scoped lookup | A | TF |
| Actions | `actions_organization_variable_repositories` | Singleton organization + variable name | R | Variable rename changes parent key | Read/set selected repositories | A | TF |
| Actions | `actions_organization_variable_repository` | NK organization + variable name + repository ID | R | Parent key change changes association | Resolve repository ID at run time | A | TF |
| Actions | `actions_organization_workflow_permissions` | Singleton organization | R | None | Direct parent setting | A | TF |
| Actions | `actions_repository_access_level` | Singleton owner + repository | R | None | Direct parent setting | A | TF |
| Actions | `actions_repository_oidc_subject_claim_customization_template` | Singleton owner + repository | R | None | Direct parent setting | A | TF |
| Actions | `actions_repository_permissions` | Singleton owner + repository | R | None | Direct parent setting | A | TF |
| Actions | `actions_runner_group` | ID organization + runner_group_id | R | Name is mutable under ID | Name matching is not documented unique; selected runners and repositories also use IDs | C | TF |
| Actions | `actions_secret` | NK owner + repository + secret name | W plaintext/encrypted value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Actions | `actions_variable` | NK owner + repository + variable name | R | Name change changes key | Direct scoped lookup | A | TF |
| Actions | `workflow_repository_permissions` | Singleton owner + repository | R | None | Direct parent setting | A | TF |
| Actions | REST-only `OrganizationActionsPolicy` | ID organization + policy_id | R | Name/settings remain under ID | `actions/create-org-actions-policy` returns an ID; no natural-key endpoint | C | REST |
| Actions | REST-only `RepositoryActionsPolicy` | ID owner + repository + policy_id | R | Name/settings remain under ID | `actions/create-repo-actions-policy` returns an ID | C | REST |
| Actions | REST-only `ActionsCacheLimits` | Singleton enterprise, organization, or repository | R | None | Direct retention/storage-limit setting endpoints | A | REST |
| Actions | REST-only `ActionsArtifactLogRetention` | Singleton organization or repository | R | None | Direct artifact-and-log-retention setting endpoints | A | REST |
| Actions | REST-only `AgentsSecret` | NK organization or repository + secret name | W secret value | Rename is delete/create | Same presence-only and force-resend compromise as Actions secrets | B | REST |
| Actions | REST-only `AgentsVariable` | NK organization or repository + variable name | R | Name change changes key | Direct scoped lookup | A | REST |
| Actions | REST-only `SelfHostedRunnerLabels` | ID organization or repository + runner_id | R | Label names are set members; runner identity remains numeric | Cannot create or rediscover the runner from declared labels | C | REST |
| Dependabot and Codespaces | `codespaces_organization_secret` | NK organization + secret name | W secret value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Dependabot and Codespaces | `codespaces_organization_secret_repositories` | Singleton organization + secret name | R | Secret rename changes parent key | Read/set selected repository list | A | TF |
| Dependabot and Codespaces | `codespaces_secret` | NK owner + repository + secret name | W secret value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Dependabot and Codespaces | `codespaces_user_secret` | NK authenticated user + secret name | W secret value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Dependabot and Codespaces | `dependabot_organization_secret` | NK organization + secret name | W secret value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Dependabot and Codespaces | `dependabot_organization_secret_repositories` | Singleton organization + secret name | R | Secret rename changes parent key | Read/set selected repositories | A | TF |
| Dependabot and Codespaces | `dependabot_organization_secret_repository` | NK organization + secret name + repository ID | R | Parent key change changes association | Resolve repository ID at run time | A | TF |
| Dependabot and Codespaces | `dependabot_secret` | NK owner + repository + secret name | W secret value | Rename is delete/create | Presence-only plus explicit forced resend | B | TF |
| Dependabot and Codespaces | REST-only `OrganizationCodespacesAccess` | Singleton organization | R | None | Direct `codespaces/set-codespaces-access` setting | A | REST |
| Dependabot and Codespaces | REST-only `DependabotRepositoryAccess` | Singleton enterprise or organization | R | None | Direct repository-access setting | A | REST |
| Environments | `repository_deployment_branch_policy` | ID owner + repository + environment + branch_policy_id | R | Pattern can change under ID | Pattern/type uniqueness is not promised; content matching is ambiguous | C | TF |
| Environments | `repository_environment` | NK owner + repository + environment name | R | No rename endpoint; name change means new environment | Direct environment-name path; explicit old-key delete | A | TF |
| Environments | `repository_environment_deployment_policy` | ID owner + repository + environment + policy_id | R | Pattern can change under ID | Same ID-only policy problem | C | TF |
| Environments | REST-only `RepositoryDeploymentProtectionRule` | ID owner + repository + environment + protection_rule_id | R | App/rule remains under ID | Create/delete endpoints use assigned ID | C | REST |
| Organization settings and roles | `organization_block` | NK organization + username | R | Login change changes association | Direct blocked-user membership endpoint | A | TF |
| Organization settings and roles | `organization_custom_properties` | NK organization + property name | R | Property rename changes key | Direct property-name path | A | TF |
| Organization settings and roles | `organization_custom_role` | ID organization + role_id | R | Name is mutable under ID | Deprecated and ID-only; name matching is not guaranteed unique | C | TF |
| Organization settings and roles | `organization_repository_role` | ID organization + role_id | R | Name is mutable under ID | Custom repository-role CRUD uses the assigned ID | C | TF |
| Organization settings and roles | `organization_role` | ID organization + role_id | R | Name is mutable under ID | Organization-role lookup uses role ID | C | TF |
| Organization settings and roles | `organization_role_team` | ID role_id + team slug | R | Team slug or role changes association | Association has a tuple, but its role parent has no natural key | C | TF |
| Organization settings and roles | `organization_role_team_assignment` | ID team slug + role_id | R | Same | Deprecated form of the same ID-parent association | C | TF |
| Organization settings and roles | `organization_role_user` | ID role_id + login | R | Login or role change changes association | Role parent remains ID-only | C | TF |
| Organization settings and roles | `organization_settings` | Singleton organization | R | Organization login rename changes parent key | Direct parent settings | A | TF |
| Organization settings and roles | REST-only `CodeSecurityConfiguration` | ID enterprise or organization + configuration_id | R | Name is mutable under ID | Create/update/delete use the assigned configuration ID | C | REST |
| Organization settings and roles | REST-only `OrganizationIssueField` | ID organization + issue_field_id | R | Name is mutable under ID | `orgs/create-issue-field` returns an ID | C | REST |
| Organization settings and roles | REST-only `OrganizationIssueType` | ID organization + issue_type_id | R | Name is mutable under ID | `orgs/create-issue-type` returns an ID | C | REST |
| Organization settings and roles | REST-only `OrganizationPrivateRegistry` | NK organization + secret_name | W credentials | Rename is delete/create | Addressable, but credentials require presence-only plus forced resend | B | REST |
| Organization settings and roles | REST-only `SecretScanningCustomPattern` | ID organization or repository + pattern_id | R | Name/pattern can change under ID | Bulk create then update by assigned pattern ID | C | REST |
| Organization settings and roles | REST-only `OrganizationImmutableReleasesSettings` | Singleton organization | R | None | Direct organization setting and selected-repository set | A | REST |
| Organization settings and roles | REST-only `HostedComputeNetworkConfiguration` | ID organization + network_configuration_id | R | Name/settings remain under ID | Create returns assigned ID | C | REST |
| Organization settings and roles | REST-only `CopilotCodingAgentPermissions` | Singleton enterprise or organization | R | None | Direct policy and selected-repository settings | A | REST |
| Organization settings and roles | REST-only `OrganizationInteractionLimits` | Singleton organization | R | None | Direct set/remove endpoint | A | REST |
| Organization settings and roles | REST-only `OrganizationSecurityProductEnablement` | Singleton organization + security product | R | Product is enum-like | Direct enable/disable endpoint | A | REST |
| Teams and membership | `emu_group_mapping` | NK enterprise group ID + team slug | R | Team rename changes slug | Resolve mapping from the declared external group and team | A | TF |
| Teams and membership | `membership` | NK organization + username | R | Login change changes association | Direct membership endpoint | A | TF |
| Teams and membership | `organization_security_manager` | NK organization + team slug | R | Team rename changes slug | REST endpoint is `/security-managers/teams/{team_slug}` even though TF stores team ID | A | TF |
| Teams and membership | `team` | NK organization + team slug | R | Name change derives a new slug | Direct lookup by slug; rename needs an explicit old slug or is new plus unmanaged old | A | TF |
| Teams and membership | `team_members` | Singleton organization + team slug | R | Team rename changes parent key | Explicit authoritative member set | A | TF |
| Teams and membership | `team_membership` | NK organization + team slug + username | R | Team or login rename changes association | Direct membership endpoint | A | TF |
| Teams and membership | `team_repository` | NK organization + team slug + owner + repository | R | Parent key change changes association | Direct repository association | A | TF |
| Teams and membership | `team_settings` | Singleton organization + team slug | R | Team rename changes parent key | Resolve slug to GraphQL node ID per run | A | TF |
| Teams and membership | `team_sync_group_mapping` | Singleton organization + team slug | R | Team rename changes parent key | Read/set the team's IdP group mapping | A | TF |
| Webhooks, keys, and apps | `app_installation_repositories` | Singleton installation_id | R | Installation is an externally assigned parent ID | Manage the complete repository selection for an explicitly supplied installation | A | TF |
| Webhooks, keys, and apps | `app_installation_repository` | NK installation_id + repository | R | Repository key change changes association | Direct installation association | A | TF |
| Webhooks, keys, and apps | `organization_webhook` | ID organization + hook_id | W configuration secret | URL and secret rotate under retained ID | URL has no organization-level uniqueness contract and duplicate matches are ambiguous; secret equality is also unknowable | C | TF |
| Webhooks, keys, and apps | `repository_deploy_key` | ID owner + repository + key_id | R | Keys have no update endpoint; rotation is replacement | Match by key material only if uniqueness and canonicalization are proven; OpenAPI does not provide that guarantee | C | TF |
| Webhooks, keys, and apps | `repository_webhook` | ID owner + repository + hook_id | W configuration secret | URL and secret rotate under retained ID | `repos/create-webhook` explicitly allows hooks to share a config when events do not overlap; the secret is also masked | C | TF |
| Webhooks, keys, and apps | REST-only `AppWebhookConfig` | Singleton authenticated app | W configuration secret | URL or secret changes in place | Presence of app config is addressable; secret needs explicit forced resend | B | REST |
| Projects, issues, and releases | `issue` | ID owner + repository + issue number | R | Title is mutable and non-unique | Content/operational object rather than infrastructure setting; title/body matching can select the wrong assigned number | C | TF |
| Projects, issues, and releases | `issue_label` | NK owner + repository + label name | R | Case-only change is a display-name update under a folded key | Direct label-name endpoint; provider needs tested case normalization | A | TF |
| Projects, issues, and releases | `issue_labels` | Singleton owner + repository | R | Label key changes are explicit set changes | Authoritative collection only when explicitly selected | A | TF |
| Projects, issues, and releases | `organization_project` | ID project ID; classic API retired | R when API existed | Title was mutable | Resource no longer works against the retired classic Projects API | C | TF |
| Projects, issues, and releases | `project_card` | ID card or node ID; classic API retired | R when API existed | Content/location mutable under ID | Retired and ID-only | C | TF |
| Projects, issues, and releases | `project_column` | ID column ID; classic API retired | R when API existed | Name mutable under ID | Retired and ID-only | C | TF |
| Projects, issues, and releases | `release` | NK owner + repository + tag through `repos/get-release-by-tag` | R | `tag_name` can change under retained release ID | Find by tag, then use ephemeral release ID; key rename needs explicit old tag | A | TF |
| Projects, issues, and releases | `repository_milestone` | ID owner + repository + milestone number | R | Title is mutable and non-unique | Assigned number is required; title matching is unsafe | C | TF |
| Projects, issues, and releases | `repository_project` | ID project ID; classic API retired | R when API existed | Title was mutable | Retired and ID-only | C | TF |
| Projects, issues, and releases | `repository_pull_request` | ID owner + repository + pull number | R | Branches/title can change; multiple historical PRs exist | Base/head/title is not a durable unique key; this is an operational object | C | TF |
| Projects, issues, and releases | REST-only `ProjectsV2FieldItemView` | ID project number plus field, item, or view ID | R | Names/content mutable under IDs | Create operations return assigned IDs; content matching is ambiguous | C | REST |
| Projects, issues, and releases | REST-only `ReleaseAsset` | ID owner + repository + asset_id | R | Name is mutable under ID | Upload returns assigned ID; duplicate-name/error behavior is not a natural lookup contract | C | REST |
| Enterprise | `enterprise_actions_permissions` | Singleton enterprise slug | R | None | Direct parent setting | A | TF |
| Enterprise | `enterprise_actions_runner_group` | ID enterprise + runner_group_id | R | Name is mutable under ID | No documented unique lookup by desired fields | C | TF |
| Enterprise | `enterprise_actions_workflow_permissions` | Singleton enterprise slug | R | None | Direct parent setting | A | TF |
| Enterprise | `enterprise_ip_allow_list_entry` | ID GraphQL node ID | R | CIDR/name can change under ID | CIDR uniqueness is not promised; content matching can be ambiguous | C | TF |
| Enterprise | `enterprise_organization` | NK enterprise slug + organization login | R | Organization rename changes key | Import already resolves slug to ephemeral node ID | A | TF |
| Enterprise | `enterprise_security_analysis_settings` | Singleton enterprise slug | R | None | Direct parent setting | A | TF |
| Enterprise | REST-only `EnterpriseCodeSecurityConfiguration` | ID enterprise + configuration_id | R | Name mutable under ID | Create/update/delete use assigned ID | C | REST |
| Enterprise | REST-only `EnterpriseCopilotCodingAgentPolicy` | Singleton enterprise | R | None | Direct policy endpoint | A | REST |
| User | `user_gpg_key` | ID gpg_key_id | R public key, but exact armored serialization is not stable identity | Key cannot be updated | Fingerprint matching would need a documented canonical-key guarantee; the TF importer is intentionally absent | C | TF |
| User | `user_invitation_accepter` | Action invitation_id | R for pending invitation only | Accepted invitation disappears and later reads may be 404 | One-shot command with a random/provider ID, not convergent managed state | C | TF |
| User | `user_ssh_key` | ID key_id | R | Title changes under ID; key rotation is replacement | Title is non-unique; public-key content matching and duplicate rejection are not a documented identity contract | C | TF |
| User | REST-only `UserSshSigningKey` | ID ssh_signing_key_id | R | Title/key replacement remains under ID | Same unsafe content-match problem as SSH authentication keys | C | REST |
| User | REST-only `UserEmail` | NK authenticated user + email | R | Email address change is remove/add | List membership gives a direct value key | A | REST |
| User | REST-only `UserSocialAccount` | NK authenticated user + URL | R | URL change is remove/add | List membership gives a direct value key | A | REST |

### Reproducible counts

The following command checks that every TF document appears exactly once in
the table, then derives both the address/readability counts and the mutually
exclusive reconciliation buckets from the table itself:

```sh
TF_PROVIDER=${TF_PROVIDER:?set this to an integrations/terraform-provider-github checkout}
python3 - "$TF_PROVIDER" notes/specs/2026-10-28-4043-stateless-mode-design.md <<'PY'
import collections
import pathlib
import re
import sys

provider = pathlib.Path(sys.argv[1])
document = pathlib.Path(sys.argv[2]).read_text()
rows = []
rest_rows = []
for line in document.splitlines():
    if not line.startswith("|"):
        continue
    columns = [column.strip() for column in line.strip().strip("|").split("|")]
    if len(columns) != 8:
        continue
    if columns[7] == "REST":
        rest_rows.append((columns[2], columns[3], columns[6]))
        continue
    if columns[7] != "TF":
        continue
    match = re.fullmatch(r"`([a-z0-9_]+)`", columns[1])
    if match:
        rows.append((match.group(1), columns[2], columns[3], columns[6]))

documented = {path.stem for path in (provider / "docs/resources").glob("*.md")}
surveyed = {name for name, _, _, _ in rows}
assert len(rows) == len(surveyed) == 88
assert surveyed == documented, (sorted(documented - surveyed), sorted(surveyed - documented))

address = collections.Counter(
    "natural_or_singleton" if value.startswith(("NK ", "Singleton ")) else "id_or_action"
    for _, value, _, _ in rows
)
masked = sum(readability.startswith("W ") for _, _, readability, _ in rows)
buckets = collections.Counter(bucket for _, _, _, bucket in rows)
print(address)
print("write_only_or_masked", masked)
print(buckets)
print("rest_candidate_families", len(rest_rows))
print(collections.Counter(bucket for _, _, bucket in rest_rows))
PY
```

Output:

```text
Counter({'natural_or_singleton': 60, 'id_or_action': 28})
write_only_or_masked 10
Counter({'A': 51, 'C': 29, 'B': 8})
rest_candidate_families 32
Counter({'A': 16, 'C': 13, 'B': 3})
```

The REST-only rows are not folded into the exact 88-type numbers because
several rows are families spanning multiple scopes and Carina resource
granularity has not been designed. As table families they add 16 A, three B,
and 13 C rows; none can remove a C blocker from the 88-type baseline.

## Why the C Types Block a Provider-Wide Capability

### Server IDs cannot be reconstructed safely

The Terraform implementation confirms that these are not merely cosmetic
state IDs.

- Repository webhooks save the ID returned by create and use it for every
  later read, update, and delete
  (`integrations/terraform-provider-github:github/resource_github_repository_webhook.go:106-184`,
  `integrations/terraform-provider-github:github/resource_github_repository_webhook.go:191-225`).
  Organization webhooks do the same
  (`integrations/terraform-provider-github:github/resource_github_organization_webhook.go:86-180`,
  `integrations/terraform-provider-github:github/resource_github_organization_webhook.go:183-228`).
  The OpenAPI paths are `/repos/{owner}/{repo}/hooks/{hook_id}` and
  `/orgs/{org}/hooks/{hook_id}`. The only obvious content candidate is
  `config.url`, but OpenAPI `repos/create-webhook` explicitly permits multiple
  repository hooks to share the same config when their events do not overlap;
  organization-hook URL uniqueness is not promised either. Two hooks with the
  same URL make update and delete ambiguous.
- Deploy-key create returns an ID, and get/delete require that ID
  (`integrations/terraform-provider-github:github/resource_github_repository_deploy_key.go:68-159`;
  OpenAPI `repos/create-deploy-key` and
  `/repos/{owner}/{repo}/keys/{key_id}`). GitHub may reject a duplicate key in
  practice, but that behavior is not stated in the checked OpenAPI description
  and was not acceptance-tested here. Matching normalized key material would
  therefore be an assumption. There is no update endpoint, so rotation also
  needs a correctly identified delete.
- Runner groups retain the returned numeric ID
  (`integrations/terraform-provider-github:github/resource_github_actions_runner_group.go:140-227`;
  OpenAPI `/orgs/{org}/actions/runner-groups/{runner_group_id}`). Name is
  readable and mutable, but the API description does not declare it a unique
  lookup key.
- Repository and organization ruleset create/read/update retain
  `ruleset_id`
  (`integrations/terraform-provider-github:github/resource_github_repository_ruleset.go:711-871`,
  `integrations/terraform-provider-github:github/resource_github_organization_ruleset.go:771-918`;
  OpenAPI `/repos/{owner}/{repo}/rulesets/{ruleset_id}` and
  `/orgs/{org}/rulesets/{ruleset_id}`). gh-infra documents names as unique
  within a repository, but the checked REST description only documents the
  ID-addressed operation. Treating name as provider-enforced identity needs an
  acceptance test and an upstream guarantee; third-party behavior alone is
  not enough for Carina's provider-wide type contract.
- Custom repository roles keep `role_id` across get, update, and delete
  (`integrations/terraform-provider-github:github/resource_github_organization_repository_role.go:95-219`).
  Organization roles and their team/user associations inherit the same
  problem.
- Milestones are read, updated, and deleted by their assigned number even
  though title is mutable
  (`integrations/terraform-provider-github:github/resource_github_repository_milestone.go:128-245`;
  OpenAPI `/repos/{owner}/{repo}/milestones/{milestone_number}`). Issue titles
  and milestone titles are not unique. Issues likewise store the assigned
  issue number (`integrations/terraform-provider-github:github/resource_github_issue.go:144-171`).

An explicit ID attribute does not solve create. Before create it is absent;
after a successful remote create whose response is lost, retry still has no
way to distinguish the created object from another matching object. A
user-supplied ID mode can manage an already existing object, but it is an
import handle, not an all-lifecycle stateless resource.

### Content matching has concrete failure modes

| Proposed fallback | Failure |
| --- | --- |
| Webhook by `config.url` | `repos/create-webhook` allows shared config for non-overlapping events; duplicate URLs can select more than one hook. URL change also looks like delete plus create. |
| Deploy or user key by public key | Duplicate rejection and canonicalization were not verified. A changed key is replacement, and the old key must still be identified for deletion. |
| Ruleset, runner group, custom role, or policy by name | Mutations use assigned IDs; names are mutable, and official uniqueness was not established for the surveyed endpoints. |
| Milestone or issue by title | Titles are explicitly mutable and may be duplicated. |
| Autolink by prefix or deployment policy by pattern | Import/list matching exists, but the REST description does not promise uniqueness. |
| IP allow-list entry by CIDR | The GraphQL resource uses a node ID; duplicate/overlapping CIDR policy is not an identity guarantee. |
| Project, column, card, field, item, or view by display text | Display text is mutable and non-unique; classic Projects endpoints are also retired. |

These are the exact failure modes prohibited by `CLAUDE.md:83-135`: a mutable
user field would occupy an equality-bearing remote-identity position. Wrapping
it in a newtype does not create a provider uniqueness guarantee.

### Write-only values are not drift-detectable

The Actions secret API returns metadata, not the value. The Terraform provider
writes `plaintext_value` or encrypted input but later reads only timestamps
(`integrations/terraform-provider-github:github/resource_github_actions_secret.go:161-250`).
Its Codespaces secret implementation explicitly compares the remote
`updated_at` with a timestamp persisted in state and forces recreation when
they differ (`integrations/terraform-provider-github:github/resource_github_codespaces_secret.go:110-165`).

Without state, `updated_at` cannot replace the value hash. It can say that
someone wrote the secret at a time, but not whether the current plaintext
equals the desired plaintext, who wrote it, or whether a retry already sent
the desired value. The honest stateless contract is B: detect missing only;
show no value diff for an existing secret; and expose an explicit force action
that resends selected secrets. This is the same compromise gh-infra documents
in its [secrets and variables page](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/secrets-variables.md).

Webhook configuration adds the same problem to a C type. GitHub returns the
secret masked; the Terraform provider deliberately copies the previous state
value over the masked response
(`integrations/terraform-provider-github:github/resource_github_repository_webhook.go:119-126`,
`integrations/terraform-provider-github:github/resource_github_repository_webhook.go:172-184`).
Stateless matching by URL would therefore combine ambiguous identity with an
undetectable secret rotation.

The GPG-key resource is another C case. It creates and subsequently reads by
the server ID, while the Terraform read restores only the derived key ID
(`integrations/terraform-provider-github:github/resource_github_user_gpg_key.go:43-103`).
Its Terraform documentation says import is unavailable because the API does
not return the previously uploaded key
(`integrations/terraform-provider-github:docs/resources/user_gpg_key.md:34-36`).
The newer checked OpenAPI schema for
`users/get-gpg-key-for-authenticated-user` does declare `public_key` and
nullable `raw_key`, so this survey does not count the type as masked. That
could support semantic value comparison after parsing, but it does not turn
the assigned `{gpg_key_id}` into a provider-enforced desired-input key.

### Absence and unreadable can be indistinguishable

A stateless plan must decide whether to create when lookup says the object is
missing. That decision is unsafe if lack of permission has the same response.
The live observation on `carina-rs` is direct evidence: listing organization
webhooks without `admin:org_hook` returned HTTP 404 with the ordinary `Not
Found` body. By contrast, organization Actions secret and variable listing
returned explicit HTTP 403 permission errors.

The provider cannot globally interpret 404 as absent or globally interpret it
as forbidden. Per-operation permission knowledge can fail closed, but for the
organization webhook endpoint the response alone contains no proof of
absence. gh-infra's correct rule is to leave unreadable settings out of the
plan rather than compare them as absent, documented in its
[principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md).
That safety rule means the type cannot promise discovery for all valid GitHub
authentication configurations.

### Natural keys still do not infer renames

GitHub documents the `owner` and `repo` REST parameters as case-insensitive in
the OpenAPI parameter descriptions for `repos/get` and the label endpoints.
The label-name parameter itself has no such statement. gh-infra documents and
tests case-insensitive label matching: `Bug` against remote `bug` is an update
of the display name, and duplicate manifest labels differing only by case are
rejected ([labels](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md)).
Carina would need an acceptance test before making that a provider contract.

Other key changes have no such same-key interpretation. A repository rename
may leave an old-name redirect, but desired input contains only the new name;
it does not tell lookup which old name to try. A team name change derives a
new slug. Environment, branch, variable, and secret names likewise change the
address. Stateless semantics can safely treat those as a new object while
leaving the old object unmanaged, or require an explicit old-key alias. They
cannot infer rename from the new desired key.

## Prior Art: gh-infra

gh-infra demonstrates that a deliberately scoped GitHub tool can be useful
without a state file. GitHub is its source of truth; each plan fetches live
state and diffs it against YAML. It has no lock, documents last-write-wins,
recommends CI concurrency groups or a merge queue, and cannot detect a
repository removed from YAML
([state management](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)).

Its ownership model is deliberately partial. Omitted fields are not compared
or sent; explicit empty values clear fields. Collections are additive by
default. Authoritative deletion is opt-in only for labels, rulesets, and
branch protection, and only when that collection appears. Whole-object APIs
are handled by reading current state, overlaying declared fields, and
preserving settings that gh-infra does not model. Unreadable settings are
excluded from the plan, and apply compares the returned object with what it
sent
([principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)).

gh-infra also makes the compromises visible:

| Concern | gh-infra behavior | Relevance here |
| --- | --- | --- |
| Labels | Case-insensitive name match; case-only change is a rename; aliases remain future work ([labels](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/labels.md)) | A provider can own normalization, but old-key aliases are still unsolved |
| Rulesets | Name is required and documented by gh-infra as unique per repository; team/app slugs are resolved via API at run time; whole-object update overlays declared fields ([rulesets](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/rulesets.md)) | Useful implementation evidence, but not an official uniqueness guarantee for Carina's provider type |
| Secrets | Missing-only detection; no changed-value diff; `--force-secrets` resends all selected secrets ([secrets and variables](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/resources/repository/secrets-variables.md)) | This is bucket B, not ordinary drift detection |
| Deletion | Omission stops management; only selected collections have authoritative reconcile ([principles](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/introduction/principles.md)) | It intentionally avoids general ownership and delete discovery |
| Locking | None; last write wins and the next plan exposes inconsistency ([state management](https://github.com/babarot/gh-infra/blob/main/docs/src/content/docs/internals/state-management.md)) | Operationally valid for that tool, but not equivalent to Carina's state lock |

This prior art does not meet the provider-wide rule. gh-infra defines a narrow
manifest model and assigns resource-specific behavior: some collections are
authoritative, some are always additive, secrets are presence-only, deleted
top-level resources are undetected, and unsupported GitHub objects simply are
not part of its model. That is a coherent product choice. It is not proof that
every type a comprehensive Carina GitHub provider might ship has a stateless
identity and recovery contract.

Carina should retain two ideas even though this proposal is rejected. First,
unreadable must be typed separately from absent. Second, any future stateless
field model must make omission mean “unmanaged now,” not silently copy
state-backed behavior. State-backed Carina can tell that a field was authored
previously: the differ projects never-authored fields away but marks a
previously authored, now-omitted field as changed
(`carina-core/src/differ/comparison.rs:838-861`), and the update patch emits
`PatchOp::Remove` (`carina-core/src/provider.rs:710-765`). Stateless execution
has no previous authored-field tree, so dropping a field would stop managing
it; an explicit empty value would clear it.

## Why Backend Omission Is Not the Opt-in

If stateless mode is ever reconsidered, it must remain an explicit
`backend none {}` selection. Omitting `backend` already means the implicit
local state backend; it does not mean no state.

Plan states this directly and reads the default backend when no configuration
is selected (`carina-cli/src/commands/plan.rs:2363-2369`,
`carina-cli/src/commands/plan.rs:2428-2436`). Backend construction turns
`None` into a `LocalBackend` at `carina.state.json`
(`carina-state/src/backends/mod.rs:29-37`). Apply and destroy both verify and
resolve that default backend before proceeding
(`carina-cli/src/commands/apply/mod.rs:1383-1390`,
`carina-cli/src/commands/destroy.rs:100-123`).

Reinterpreting omission would silently switch every existing project without
an explicit backend block from local state to stateless operation. It would
also make accidentally deleting `backend.crn` change the execution model
instead of producing backend-drift protection. Explicit syntax is therefore a
compatibility and safety boundary, not verbosity for its own sake.

## What an Accepted Design Would Have Required

No implementation work should begin under the current verdict. If the
provider-wide constraint or GitHub APIs change, an accepted design would still
need all of the following:

- a provider-level tagged schema set as described above, validated identically
  by CLI and LSP;
- explicit `backend none {}` and separate `StatelessPlan` and
  `StatelessApply` typestates with no state snapshot, lineage, serial, saved
  identifier, or backend lock field;
- a provider operation to find by typed natural key, with found, absent, and
  unreadable outcomes, plus paginated list operations for explicitly
  authoritative scopes;
- typed managed-field masks and whole-object overlay operations so omitted and
  unmodeled GitHub fields survive replacement-style APIs;
- post-write observation, create-recovery contracts, parent-key ordering, and
  explicit key-alias syntax for supported renames;
- honest B-type secret operations: presence-only plans and an explicit forced
  resend, never a fabricated equality result;
- a protocol and WIT version bump. The current protocol is version 12
  (`carina-provider-protocol/src/lib.rs:6-11`), current WIT exposes neither
  find-by-key nor list (`carina-plugin-wit/wit/provider.wit:39-73`), and the
  current resource schema has no provider execution-model tag
  (`carina-core/src/schema/mod.rs:6698-6773`,
  `carina-provider-protocol/src/types.rs:1100-1129`).

Counterpart changes would be required in `carina-plugin-wit`, `carina`, and
the stateless provider repository. Stateful `carina-provider-aws` and
`carina-provider-awscc` would need only the coordinated protocol rebuild; they
would remain provider-wide stateful, with no per-type stateless flags.

## Future Reconsideration

Reopen this decision only if at least one premise changes:

1. The provider-wide rule is relaxed, or a separate deliberately limited
   provider is acceptable. The existing `github` provider still must not mix
   modes.
2. GitHub adds documented, provider-enforced natural lookup keys or
   client-supplied idempotency keys for the C families, including webhooks,
   deploy keys, runner groups, rulesets, roles, milestones, projects, and
   environment policies.
3. Product requirements explicitly accept import-only resources whose remote
   server ID must be written in configuration and which cannot support create.
   Those must be a distinct type, not a weakened managed resource.
4. The project accepts B semantics for secrets and masked webhook fields as a
   typed capability: presence-only plus explicit resend, with no claim of
   value drift detection.
5. Provider acceptance tests prove normalization, uniqueness, permission
   behavior, interrupted-create recovery, pagination, and rename behavior for
   every shipped type.

Until then, the honest answer to carina-rs/carina#4043 is that stateless GitHub
management is useful for a selected subset, as gh-infra demonstrates, but is
not realistic for every GitHub resource under a provider-wide all-or-nothing
capability. The design is rejected rather than weakening Carina's identity and
type-safety rules.
