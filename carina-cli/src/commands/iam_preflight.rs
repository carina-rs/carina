use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use aws_config::BehaviorVersion;
use aws_sdk_iam::operation::simulate_principal_policy::SimulatePrincipalPolicyError;
use aws_sdk_iam::types::PolicyEvaluationDecisionType;
use carina_core::effect::{Effect, PlanOp};
use carina_core::plan::Plan;
use carina_core::provider::Provider;
use carina_core::resource::ResourceId;
use colored::Colorize;
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IamPreflightResult {
    Skipped(IamPreflightSkipped),
    Checked(IamPreflightReport),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IamPreflightSkipped {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IamPreflightReport {
    pub actor_arn: String,
    pub method: IamCheckMethod,
    pub source_providers: Vec<String>,
    pub missing_by_effect: Vec<MissingEffectActions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IamCheckMethod {
    SimulatePrincipalPolicy,
    DocumentFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingEffectActions {
    pub effect: EffectAddress,
    pub missing_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectAddress {
    pub resource: String,
    pub op: PlanOp,
}

impl PartialOrd for EffectAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EffectAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        self.resource
            .cmp(&other.resource)
            .then_with(|| plan_op_rank(self.op).cmp(&plan_op_rank(other.op)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequiredAction {
    pub effect: EffectAddress,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimulationResult {
    denied_actions: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentFallbackResult {
    allowed_actions: BTreeSet<String>,
}

/// IAM principal ARN suitable for `iam:SimulatePrincipalPolicy`,
/// `iam:GetRolePolicy`, and `iam:ListAttachedRolePolicies`.
///
/// STS session ARNs are not valid principal ARNs. Construct this value via
/// `PolicySourceArn::from_caller_identity_arn`, which converts assumed-role and
/// federated-user session ARNs to their underlying IAM principal ARN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicySourceArn(String);

impl PolicySourceArn {
    pub(crate) fn from_caller_identity_arn(arn: &str) -> Result<Self, String> {
        let parsed = ParsedArn::parse(arn)?;
        if parsed.service == "sts" {
            if let Some(rest) = parsed.resource.strip_prefix("assumed-role/") {
                let (role_name, _) = rest
                    .split_once('/')
                    .ok_or_else(|| format!("unsupported STS caller identity ARN: {arn}"))?;
                if role_name.is_empty() {
                    return Err(format!("unsupported STS caller identity ARN: {arn}"));
                }
                return Ok(Self(format!(
                    "arn:{}:iam::{}:role/{}",
                    parsed.partition, parsed.account, role_name
                )));
            }
            if let Some(user_name) = parsed.resource.strip_prefix("federated-user/") {
                if user_name.is_empty() {
                    return Err(format!("unsupported STS caller identity ARN: {arn}"));
                }
                return Ok(Self(format!(
                    "arn:{}:iam::{}:user/{}",
                    parsed.partition, parsed.account, user_name
                )));
            }
            return Err(format!("unsupported STS caller identity ARN: {arn}"));
        }
        if parsed.service == "iam" {
            let supported = parsed
                .resource
                .strip_prefix("role/")
                .or_else(|| parsed.resource.strip_prefix("user/"))
                .or_else(|| parsed.resource.strip_prefix("group/"));
            if supported.is_some_and(|name| !name.is_empty()) {
                return Ok(Self(arn.to_string()));
            }
        }
        Err(format!("unsupported caller identity ARN: {arn}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn role_name(&self) -> Option<&str> {
        ParsedArn::parse(&self.0)
            .ok()
            .and_then(|parsed| parsed.resource.strip_prefix("role/"))
            .and_then(|name| name.rsplit('/').next())
            .filter(|name| !name.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedArn<'a> {
    partition: &'a str,
    service: &'a str,
    account: &'a str,
    resource: &'a str,
}

impl<'a> ParsedArn<'a> {
    fn parse(arn: &'a str) -> Result<Self, String> {
        let mut parts = arn.splitn(6, ':');
        let prefix = parts.next();
        let partition = parts.next();
        let service = parts.next();
        let _region = parts.next();
        let account = parts.next();
        let resource = parts.next();
        let (Some(prefix), Some(partition), Some(service), Some(account), Some(resource)) =
            (prefix, partition, service, account, resource)
        else {
            return Err(format!("invalid ARN: {arn}"));
        };
        if prefix == "arn"
            && !partition.is_empty()
            && !service.is_empty()
            && !account.is_empty()
            && !resource.is_empty()
        {
            return Ok(Self {
                partition,
                service,
                account,
                resource,
            });
        }
        Err(format!("invalid ARN: {arn}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SimulateError {
    NeedsFallback(String),
    Other(String),
}

pub(crate) async fn run_iam_preflight(
    plan: &Plan,
    provider: &dyn Provider,
    strict: bool,
) -> IamPreflightResult {
    let required = collect_required_actions(plan, provider);
    if required.is_empty() {
        return IamPreflightResult::Checked(IamPreflightReport {
            actor_arn: String::new(),
            method: IamCheckMethod::SimulatePrincipalPolicy,
            source_providers: plan_provider_names(plan),
            missing_by_effect: Vec::new(),
        });
    }

    let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let sts_client = aws_sdk_sts::Client::new(&config);
    let actor_arn = match resolve_actor_arn(&sts_client).await {
        Ok(actor) => actor,
        Err(e) => {
            return IamPreflightResult::Skipped(IamPreflightSkipped {
                reason: format!(
                    "Warning: IAM preflight check skipped because AWS caller identity could not be resolved ({e})."
                ),
            });
        }
    };

    let action_set = unique_actions(&required);
    let iam_client = aws_sdk_iam::Client::new(&config);
    let (method, missing_actions) = match simulate(&actor_arn, &action_set, &iam_client).await {
        Ok(simulated) => (
            IamCheckMethod::SimulatePrincipalPolicy,
            simulated.denied_actions,
        ),
        Err(SimulateError::NeedsFallback(_)) => {
            match document_fallback(&actor_arn, &action_set, &iam_client).await {
                Ok(fallback) => {
                    let missing = action_set
                        .iter()
                        .filter(|action| {
                            !action_allowed_by_documents(action, &fallback.allowed_actions)
                        })
                        .cloned()
                        .collect();
                    (IamCheckMethod::DocumentFallback, missing)
                }
                Err(e) => {
                    return IamPreflightResult::Skipped(IamPreflightSkipped {
                        reason: format!(
                            "Warning: IAM preflight check skipped for actor {} because IAM policy simulation was denied and IAM policies could not be read for fallback ({e}). \
                             The actor needs `iam:SimulatePrincipalPolicy` (with `Resource = {}`) OR `iam:GetRolePolicy` + `iam:ListAttachedRolePolicies` for the fallback path. \
                             Add the grant to enable --check-iam.",
                            actor_arn.as_str(),
                            actor_arn.as_str(),
                        ),
                    });
                }
            }
        }
        Err(SimulateError::Other(e)) => {
            return IamPreflightResult::Skipped(IamPreflightSkipped {
                reason: format!(
                    "Warning: IAM preflight check skipped because IAM policy simulation failed ({e})."
                ),
            });
        }
    };

    let report = IamPreflightReport {
        actor_arn: actor_arn.as_str().to_string(),
        method,
        source_providers: plan_provider_names(plan),
        missing_by_effect: group_missing_by_effect(&required, &missing_actions),
    };

    if strict && !report.missing_by_effect.is_empty() {
        return IamPreflightResult::Checked(report);
    }

    IamPreflightResult::Checked(report)
}

pub(crate) fn collect_required_actions(
    plan: &Plan,
    provider: &dyn Provider,
) -> Vec<RequiredAction> {
    let mut required = Vec::new();
    for effect in plan.effects() {
        for (id, op) in effect_required_ops(effect) {
            let actions = provider.required_permissions(&id, op);
            for action in actions {
                required.push(RequiredAction {
                    effect: EffectAddress {
                        resource: id.human().to_string(),
                        op,
                    },
                    action,
                });
            }
        }
    }
    required
}

fn effect_required_ops(effect: &Effect) -> Vec<(ResourceId, PlanOp)> {
    match effect {
        Effect::Read { resource } => vec![(resource.id.clone(), PlanOp::Read)],
        Effect::Create(resource) => vec![(resource.id.clone(), PlanOp::Create)],
        Effect::Update { id, .. } => vec![(id.clone(), PlanOp::Update)],
        Effect::Replace { id, to, .. } => {
            vec![
                (id.clone(), PlanOp::Delete),
                (to.id.clone(), PlanOp::Create),
            ]
        }
        Effect::Delete { id, .. } => vec![(id.clone(), PlanOp::Delete)],
        Effect::Import { id, .. } => vec![(id.clone(), PlanOp::Read)],
        Effect::DeferredCreate { template, .. } => {
            effect_required_ops(&Effect::Create(template.template_resource.clone()))
        }
        Effect::DeferredReplace {
            deletes, template, ..
        } => {
            let mut ops: Vec<_> = deletes
                .iter()
                .map(|delete| (delete.id.clone(), PlanOp::Delete))
                .collect();
            ops.extend(effect_required_ops(&Effect::Create(
                template.template_resource.clone(),
            )));
            ops
        }
        Effect::Remove { .. } | Effect::Move { .. } | Effect::Wait { .. } => Vec::new(),
    }
}

fn effect_resource_ids(effect: &Effect) -> Vec<&ResourceId> {
    match effect {
        Effect::Read { resource } => vec![&resource.id],
        Effect::Create(resource) => vec![&resource.id],
        Effect::Update { id, .. } => vec![id],
        Effect::Replace { id, to, .. } => vec![id, &to.id],
        Effect::Delete { id, .. } => vec![id],
        Effect::Import { id, .. } => vec![id],
        Effect::Remove { id, .. } => vec![id],
        Effect::Move { from, to } => vec![from, to],
        Effect::Wait { .. } => Vec::new(),
        Effect::DeferredCreate { id, .. } => vec![id],
        Effect::DeferredReplace { deletes, id, .. } => {
            let mut ids = vec![id];
            ids.extend(deletes.iter().map(|delete| &delete.id));
            ids
        }
    }
}

fn plan_provider_names(plan: &Plan) -> Vec<String> {
    plan.effects()
        .iter()
        .flat_map(effect_resource_ids)
        .filter_map(|id| (!id.provider.is_empty()).then_some(id.provider.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn format_warnings(result: &IamPreflightResult) -> Option<String> {
    match result {
        IamPreflightResult::Skipped(skipped) => Some(format!(
            "IAM preflight findings (1 warning):\n  {}",
            skipped.reason
        )),
        IamPreflightResult::Checked(report) if report.missing_by_effect.is_empty() => None,
        IamPreflightResult::Checked(report) => {
            let mut out = String::new();
            out.push_str("IAM preflight findings (1 warning):\n");
            out.push_str("  Required permissions sourced from each provider:\n");
            for line in permission_source_lines(report) {
                out.push_str(&format!("    - {line}\n"));
            }
            match report.method {
                IamCheckMethod::SimulatePrincipalPolicy => {
                    out.push_str(&format!(
                        "  Check method: iam:SimulatePrincipalPolicy (SCPs, condition keys, exact resource ARN scopes may still affect apply)\n  Actor: {}\n",
                        report.actor_arn
                    ));
                }
                IamCheckMethod::DocumentFallback => {
                    out.push_str(&format!(
                        "  Check method: policy document fallback (weaker check: identity-policy action names only; does not evaluate SCPs, permission boundaries, condition keys, or resource scopes)\n  Actor: {}\n",
                        report.actor_arn
                    ));
                }
            }
            out.push('\n');
            for effect in &report.missing_by_effect {
                out.push_str(&format!(
                    "  {} ({})\n",
                    effect.effect.resource,
                    plan_op_label(effect.effect.op)
                ));
                for action in &effect.missing_actions {
                    out.push_str(&format!("    -> missing {action}\n"));
                }
            }
            Some(out.trim_end().to_string())
        }
    }
}

pub(crate) fn emit_warnings(result: &IamPreflightResult) {
    if let Some(warning) = format_warnings(result) {
        eprintln!("{}", warning.yellow());
    }
}

pub(crate) fn print_warnings(result: &IamPreflightResult) {
    if let Some(warning) = format_warnings(result) {
        println!("{}", warning.yellow());
    }
}

pub(crate) fn should_fail_strict(result: &IamPreflightResult) -> bool {
    matches!(
        result,
        IamPreflightResult::Checked(IamPreflightReport {
            missing_by_effect,
            ..
        }) if !missing_by_effect.is_empty()
    )
}

async fn resolve_actor_arn(sts_client: &aws_sdk_sts::Client) -> Result<PolicySourceArn, String> {
    let output = sts_client
        .get_caller_identity()
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let arn = output
        .arn()
        .ok_or_else(|| "GetCallerIdentity returned no ARN".to_string())?;
    PolicySourceArn::from_caller_identity_arn(arn)
}

fn unique_actions(required: &[RequiredAction]) -> BTreeSet<String> {
    required.iter().map(|entry| entry.action.clone()).collect()
}

async fn simulate(
    actor_arn: &PolicySourceArn,
    actions: &BTreeSet<String>,
    iam_client: &aws_sdk_iam::Client,
) -> Result<SimulationResult, SimulateError> {
    let mut denied_actions = BTreeSet::new();
    let mut marker: Option<String> = None;
    loop {
        let mut request = iam_client
            .simulate_principal_policy()
            .policy_source_arn(actor_arn.as_str())
            .resource_arns("*")
            .set_marker(marker.clone());
        for action in actions {
            request = request.action_names(action);
        }

        let output = request
            .send()
            .await
            .map_err(|e| classify_simulate_error(e.into_service_error()))?;

        denied_actions.extend(
            output
                .evaluation_results()
                .iter()
                .filter(|result| result.eval_decision() != &PolicyEvaluationDecisionType::Allowed)
                .map(|result| result.eval_action_name().to_string()),
        );

        if !output.is_truncated() {
            break;
        }
        marker = output.marker().map(str::to_string);
        if marker.is_none() {
            break;
        }
    }
    Ok(SimulationResult { denied_actions })
}

async fn document_fallback(
    actor_arn: &PolicySourceArn,
    actions: &BTreeSet<String>,
    iam_client: &aws_sdk_iam::Client,
) -> Result<DocumentFallbackResult, String> {
    let role_name = actor_arn
        .role_name()
        .ok_or_else(|| format!("actor ARN is not an IAM role: {}", actor_arn.as_str()))?;
    let mut allowed_actions = BTreeSet::new();

    let inline_policy_names = iam_client
        .list_role_policies()
        .role_name(role_name)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .policy_names()
        .to_vec();
    for policy_name in inline_policy_names {
        let output = iam_client
            .get_role_policy()
            .role_name(role_name)
            .policy_name(policy_name)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        collect_allowed_actions_from_policy(
            output.policy_document(),
            actions,
            &mut allowed_actions,
        );
    }

    let attached = iam_client
        .list_attached_role_policies()
        .role_name(role_name)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .attached_policies()
        .to_vec();
    for policy in attached {
        let Some(policy_arn) = policy.policy_arn() else {
            continue;
        };
        let policy_output = iam_client
            .get_policy()
            .policy_arn(policy_arn)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let Some(default_version_id) = policy_output
            .policy()
            .and_then(|policy| policy.default_version_id())
            .map(str::to_string)
        else {
            continue;
        };
        let version_output = iam_client
            .get_policy_version()
            .policy_arn(policy_arn)
            .version_id(default_version_id)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(document) = version_output
            .policy_version()
            .and_then(|version| version.document())
        {
            collect_allowed_actions_from_policy(document, actions, &mut allowed_actions);
        }
    }

    Ok(DocumentFallbackResult { allowed_actions })
}

fn collect_allowed_actions_from_policy(
    policy_document: &str,
    required_actions: &BTreeSet<String>,
    allowed_actions: &mut BTreeSet<String>,
) {
    let document = serde_json::from_str::<JsonValue>(policy_document)
        .or_else(|_| {
            percent_decode(policy_document).and_then(|decoded| serde_json::from_str(&decoded))
        })
        .ok();
    let Some(document) = document else {
        return;
    };
    let Some(statements) = document.get("Statement") else {
        return;
    };

    let statement_values: Vec<&JsonValue> = match statements {
        JsonValue::Array(items) => items.iter().collect(),
        value => vec![value],
    };

    for statement in statement_values {
        if statement
            .get("Effect")
            .and_then(JsonValue::as_str)
            .is_none_or(|effect| !effect.eq_ignore_ascii_case("Allow"))
        {
            continue;
        }
        let patterns = action_patterns(statement.get("Action"));
        for required in required_actions {
            if patterns
                .iter()
                .any(|pattern| action_matches(pattern, required))
            {
                allowed_actions.insert(required.clone());
            }
        }
    }
}

fn action_patterns(value: Option<&JsonValue>) -> Vec<String> {
    if let Some(JsonValue::String(action)) = value {
        return vec![action.clone()];
    }
    if let Some(JsonValue::Array(actions)) = value {
        return actions
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_string)
            .collect();
    }
    Vec::new()
}

fn action_matches(pattern: &str, action: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let action = action.to_ascii_lowercase();
    if pattern == "*" || pattern == action {
        return true;
    }
    let Some(prefix) = pattern.strip_suffix('*') else {
        return false;
    };
    action.starts_with(prefix)
}

fn action_allowed_by_documents(action: &str, allowed_actions: &BTreeSet<String>) -> bool {
    allowed_actions.contains(action)
}

fn percent_decode(input: &str) -> Result<String, serde_json::Error> {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' if index + 2 < input.len() => {
                let hex = std::str::from_utf8(&input[index + 1..index + 3]).unwrap_or("");
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    bytes.push(value);
                    index += 3;
                    continue;
                }
                bytes.push(input[index]);
                index += 1;
            }
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    serde_json::from_str::<JsonValue>(&decoded).map(|_| decoded)
}

fn group_missing_by_effect(
    required: &[RequiredAction],
    missing_actions: &BTreeSet<String>,
) -> Vec<MissingEffectActions> {
    let mut grouped: BTreeMap<EffectAddress, BTreeSet<String>> = BTreeMap::new();
    for entry in required {
        if missing_actions.contains(&entry.action) {
            grouped
                .entry(entry.effect.clone())
                .or_default()
                .insert(entry.action.clone());
        }
    }
    grouped
        .into_iter()
        .map(|(effect, actions)| MissingEffectActions {
            effect,
            missing_actions: actions.into_iter().collect(),
        })
        .collect()
}

fn classify_simulate_error(err: SimulatePrincipalPolicyError) -> SimulateError {
    let code = err
        .meta()
        .code()
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    if code == "AccessDenied" {
        return SimulateError::NeedsFallback(format_simulate_error(&err));
    }

    SimulateError::Other(format_simulate_error(&err))
}

fn format_simulate_error(err: &SimulatePrincipalPolicyError) -> String {
    let code = err.meta().code().unwrap_or("unknown");
    let message = err.meta().message().unwrap_or("no message");
    format!("AWS IAM SimulatePrincipalPolicy failed with code {code}: {message}")
}

fn permission_source_lines(report: &IamPreflightReport) -> Vec<String> {
    let mut providers: BTreeSet<&str> =
        report.source_providers.iter().map(String::as_str).collect();
    if providers.is_empty() {
        providers = report
            .missing_by_effect
            .iter()
            .filter_map(|effect| effect.effect.resource.split('.').next())
            .collect();
    }
    providers
        .into_iter()
        .map(|provider| {
            // Kept CLI-local for carina#3524 to avoid a provider/WIT contract change;
            // follow-up issue will move this onto Provider as permission_source().
            match provider {
                "awscc" => "awscc -> CloudFormation registry schema `handlers.<op>.permissions` (AWS does not guarantee completeness)".to_string(),
                "aws" => "aws -> none declared (provider does not currently report required permissions)".to_string(),
                other => format!("{other} -> provider-declared required permissions"),
            }
        })
        .collect()
}

#[cfg(test)]
fn build_simulate_input_for_test(
    actor_arn: &PolicySourceArn,
    actions: &BTreeSet<String>,
    marker: Option<String>,
) -> aws_sdk_iam::operation::simulate_principal_policy::SimulatePrincipalPolicyInput {
    let mut builder =
        aws_sdk_iam::operation::simulate_principal_policy::SimulatePrincipalPolicyInput::builder()
            .policy_source_arn(actor_arn.as_str())
            .resource_arns("*")
            .set_marker(marker);
    for action in actions {
        builder = builder.action_names(action);
    }
    builder.build().expect("simulate input should be valid")
}

fn plan_op_label(op: PlanOp) -> &'static str {
    match op {
        PlanOp::Create => "create",
        PlanOp::Read => "read",
        PlanOp::Update => "update",
        PlanOp::Delete => "delete",
    }
}

fn plan_op_rank(op: PlanOp) -> u8 {
    match op {
        PlanOp::Read => 0,
        PlanOp::Create => 1,
        PlanOp::Update => 2,
        PlanOp::Delete => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_iam::error::ErrorMetadata;
    use carina_core::effect::ChangedCreateOnly;
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::provider::{
        BoxFuture, CreateRequest, DeleteRequest, ProviderError, ProviderResult, ReadRequest,
        UpdateRequest,
    };
    use carina_core::resource::{ConcreteValue, DataSource, Resource, State, Value};

    struct PermissionProvider;

    impl Provider for PermissionProvider {
        fn name(&self) -> &str {
            "permission-test"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Err(ProviderError::not_found(id.to_string())) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> Vec<String> {
            vec![format!("test:{}:{}", plan_op_label(op), id.resource_type)]
        }
    }

    #[test]
    fn collect_required_actions_maps_effect_variants() {
        let mut plan = Plan::new();
        let read = DataSource::with_provider("awscc", "identity.User", "me", None);
        let create = Resource::with_provider("awscc", "ec2.Vpc", "new", None);
        let update_id = ResourceId::with_provider("awscc", "ec2.Subnet", "old", None);
        let update_to = Resource::with_provider("awscc", "ec2.Subnet", "old", None);
        let replace_id = ResourceId::with_provider("awscc", "s3.Bucket", "old", None);
        let replace_to = Resource::with_provider("awscc", "s3.Bucket", "new", None);
        let delete_id = ResourceId::with_provider("awscc", "iam.Role", "old", None);
        let import_id = ResourceId::with_provider("awscc", "logs.Group", "existing", None);

        plan.add(Effect::Read { resource: read });
        plan.add(Effect::Create(create));
        plan.add(Effect::Update {
            id: update_id.clone(),
            from: Box::new(State::not_found(update_id.clone())),
            to: update_to,
            changed_attributes: vec!["cidr".to_string()],
        });
        plan.add(Effect::Replace {
            id: replace_id.clone(),
            from: Box::new(State::not_found(replace_id.clone())),
            to: replace_to,
            directives: Default::default(),
            changed_create_only: ChangedCreateOnly::new(vec!["name".to_string()]).unwrap(),
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });
        plan.add(Effect::Delete {
            id: delete_id.clone(),
            identifier: "role-id".to_string(),
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
        });
        plan.add(Effect::Import {
            id: import_id,
            identifier: Value::Concrete(ConcreteValue::String("group".to_string())),
        });
        plan.add(Effect::Remove {
            id: ResourceId::with_provider("awscc", "skip.Remove", "x", None),
        });
        plan.add(Effect::Move {
            from: ResourceId::with_provider("awscc", "skip.Move", "x", None),
            to: ResourceId::with_provider("awscc", "skip.Move", "y", None),
        });

        let entries = collect_required_actions(&plan, &PermissionProvider);
        let actions: Vec<_> = entries.into_iter().map(|entry| entry.action).collect();

        assert_eq!(
            actions,
            vec![
                "test:read:identity.User",
                "test:create:ec2.Vpc",
                "test:update:ec2.Subnet",
                "test:delete:s3.Bucket",
                "test:create:s3.Bucket",
                "test:delete:iam.Role",
                "test:read:logs.Group",
            ]
        );
    }

    #[test]
    fn collect_required_actions_includes_deferred_create_template_create() {
        let template_resource =
            Resource::with_provider("aws", "route53.RecordSet", "validation_records", None);
        let mut plan = Plan::new();
        plan.add(Effect::DeferredCreate {
            id: ResourceId::new("__deferred_for", "validation_records"),
            upstream_binding: "cert".to_string(),
            template: Box::new(DeferredForExpression {
                file: None,
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.RecordSet".to_string(),
                attributes: Default::default(),
                binding_name: "validation_records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "domain_validation_options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource,
            }),
        });

        let entries = collect_required_actions(&plan, &PermissionProvider);
        let actions: Vec<_> = entries.into_iter().map(|entry| entry.action).collect();

        assert_eq!(actions, vec!["test:create:route53.RecordSet"]);
    }

    #[test]
    fn format_warnings_groups_by_effect_and_marks_fallback_limits() {
        let result = IamPreflightResult::Checked(IamPreflightReport {
            actor_arn: "arn:aws:sts::123456789012:assumed-role/deploy/session".to_string(),
            method: IamCheckMethod::DocumentFallback,
            source_providers: vec!["awscc".to_string()],
            missing_by_effect: vec![MissingEffectActions {
                effect: EffectAddress {
                    resource: "awscc.elasticloadbalancingv2.LoadBalancer alb".to_string(),
                    op: PlanOp::Create,
                },
                missing_actions: vec![
                    "iam:CreateServiceLinkedRole".to_string(),
                    "ec2:DescribeInternetGateways".to_string(),
                ],
            }],
        });

        insta::assert_snapshot!(format_warnings(&result).unwrap(), @r###"
IAM preflight findings (1 warning):
  Required permissions sourced from each provider:
    - awscc -> CloudFormation registry schema `handlers.<op>.permissions` (AWS does not guarantee completeness)
  Check method: policy document fallback (weaker check: identity-policy action names only; does not evaluate SCPs, permission boundaries, condition keys, or resource scopes)
  Actor: arn:aws:sts::123456789012:assumed-role/deploy/session

  awscc.elasticloadbalancingv2.LoadBalancer alb (create)
    -> missing iam:CreateServiceLinkedRole
    -> missing ec2:DescribeInternetGateways
"###);
    }

    #[test]
    fn classify_simulate_access_denied_uses_fallback() {
        let err = SimulatePrincipalPolicyError::generic(
            ErrorMetadata::builder()
                .code("AccessDenied")
                .message("not authorized to call SimulatePrincipalPolicy")
                .build(),
        );

        assert!(matches!(
            classify_simulate_error(err),
            SimulateError::NeedsFallback(message)
                if message.contains("code AccessDenied")
                    && !message.contains("service error")
        ));
    }

    #[test]
    fn classify_simulate_non_access_denied_reports_service_code_and_message() {
        let err = SimulatePrincipalPolicyError::generic(
            ErrorMetadata::builder()
                .code("ThrottlingException")
                .message("rate exceeded")
                .build(),
        );

        assert!(matches!(
            classify_simulate_error(err),
            SimulateError::Other(message)
                if message.contains("code ThrottlingException")
                    && message.contains("rate exceeded")
                    && !message.contains("service error")
        ));
    }

    #[test]
    fn classify_simulate_named_error_reports_service_message() {
        let err = SimulatePrincipalPolicyError::InvalidInputException(
            aws_sdk_iam::types::error::InvalidInputException::builder()
                .message("ResourceArns cannot be used with ec2:DescribeInternetGateways")
                .meta(
                    ErrorMetadata::builder()
                        .code("InvalidInput")
                        .message("ResourceArns cannot be used with ec2:DescribeInternetGateways")
                        .build(),
                )
                .build(),
        );

        assert!(matches!(
            classify_simulate_error(err),
            SimulateError::Other(message)
                if message.contains("code InvalidInput")
                    && message.contains("ResourceArns cannot be used with ec2:DescribeInternetGateways")
                    && !message.contains("service error")
        ));
    }

    #[test]
    fn policy_document_action_matching_supports_literal_and_prefix_wildcard() {
        let required = BTreeSet::from([
            "ec2:DescribeInternetGateways".to_string(),
            "iam:CreateServiceLinkedRole".to_string(),
            "s3:CreateBucket".to_string(),
        ]);
        let document = r#"{
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Action": "ec2:Describe*", "Resource": "*"},
                {"Effect": "Allow", "Action": ["iam:CreateServiceLinkedRole"], "Resource": "*"},
                {"Effect": "Deny", "Action": "s3:*", "Resource": "*"}
            ]
        }"#;
        let mut allowed = BTreeSet::new();

        collect_allowed_actions_from_policy(document, &required, &mut allowed);

        assert_eq!(
            allowed,
            BTreeSet::from([
                "ec2:DescribeInternetGateways".to_string(),
                "iam:CreateServiceLinkedRole".to_string(),
            ])
        );
    }

    #[test]
    fn policy_document_action_matching_accepts_url_encoded_iam_documents() {
        let required = BTreeSet::from(["iam:CreateServiceLinkedRole".to_string()]);
        let encoded = "%7B%22Statement%22%3A%7B%22Effect%22%3A%22Allow%22%2C%22Action%22%3A%22iam%3ACreateServiceLinkedRole%22%7D%7D";
        let mut allowed = BTreeSet::new();

        collect_allowed_actions_from_policy(encoded, &required, &mut allowed);

        assert_eq!(
            allowed,
            BTreeSet::from(["iam:CreateServiceLinkedRole".to_string()])
        );
    }

    #[test]
    fn policy_source_arn_normalizes_caller_identity_arns() {
        let cases = [
            (
                "arn:aws:sts::123456789012:assumed-role/deploy/session",
                "arn:aws:iam::123456789012:role/deploy",
            ),
            (
                "arn:aws:sts::123456789012:federated-user/alice",
                "arn:aws:iam::123456789012:user/alice",
            ),
            (
                "arn:aws:iam::123456789012:role/deploy",
                "arn:aws:iam::123456789012:role/deploy",
            ),
            (
                "arn:aws:iam::123456789012:user/alice",
                "arn:aws:iam::123456789012:user/alice",
            ),
            (
                "arn:aws:iam::123456789012:group/admins",
                "arn:aws:iam::123456789012:group/admins",
            ),
            (
                "arn:aws:sts::123456789012:assumed-role/deploy/session/with/slashes",
                "arn:aws:iam::123456789012:role/deploy",
            ),
        ];

        for (input, expected) in cases {
            let arn = PolicySourceArn::from_caller_identity_arn(input)
                .expect("caller identity ARN should normalize");

            assert_eq!(arn.as_str(), expected);
        }
    }

    #[test]
    fn policy_source_arn_rejects_unsupported_shapes() {
        assert!(PolicySourceArn::from_caller_identity_arn("not-an-arn").is_err());
    }

    #[test]
    fn policy_source_arn_role_name_only_returns_iam_roles() {
        let role =
            PolicySourceArn::from_caller_identity_arn("arn:aws:iam::123456789012:role/deploy")
                .expect("role ARN should normalize");
        let user =
            PolicySourceArn::from_caller_identity_arn("arn:aws:iam::123456789012:user/alice")
                .expect("user ARN should normalize");
        let group =
            PolicySourceArn::from_caller_identity_arn("arn:aws:iam::123456789012:group/admins")
                .expect("group ARN should normalize");

        assert_eq!(role.role_name(), Some("deploy"));
        assert_eq!(user.role_name(), None);
        assert_eq!(group.role_name(), None);
    }

    #[test]
    fn policy_source_arn_normalizes_issue_assumed_role_session() {
        let arn = PolicySourceArn::from_caller_identity_arn(
            "arn:aws:sts::151116838382:assumed-role/carina-registry-publish-deploy/1781434654872024000",
        )
        .expect("issue actor ARN should normalize");

        assert_eq!(
            arn.as_str(),
            "arn:aws:iam::151116838382:role/carina-registry-publish-deploy"
        );
    }

    #[test]
    fn simulate_request_input_uses_actor_actions_resource_star_and_marker() {
        let actions = BTreeSet::from([
            "ec2:DescribeInternetGateways".to_string(),
            "iam:CreateServiceLinkedRole".to_string(),
        ]);
        let actor = PolicySourceArn::from_caller_identity_arn(
            "arn:aws:sts::123456789012:assumed-role/deploy/session",
        )
        .expect("actor ARN should normalize");

        let input = build_simulate_input_for_test(&actor, &actions, Some("next-page".to_string()));

        assert_eq!(
            input.policy_source_arn(),
            Some("arn:aws:iam::123456789012:role/deploy")
        );
        assert_eq!(
            input.action_names(),
            &[
                "ec2:DescribeInternetGateways".to_string(),
                "iam:CreateServiceLinkedRole".to_string()
            ]
        );
        assert_eq!(input.resource_arns(), &["*".to_string()]);
        assert_eq!(input.marker(), Some("next-page"));
    }
}
