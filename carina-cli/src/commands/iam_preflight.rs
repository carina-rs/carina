use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::output_style::Colorize;
use aws_config::BehaviorVersion;
use aws_sdk_iam::operation::simulate_principal_policy::SimulatePrincipalPolicyError;
use aws_sdk_iam::types::PolicyEvaluationDecisionType;
use carina_core::effect::{Effect, PlanOp};
use carina_core::plan::Plan;
use carina_core::provider::HostProvider;
use carina_core::provider_text::RequiredPermission;
use carina_core::resource::ResourceId;
use carina_core::schema::SchemaRegistry;
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IamPreflightResult {
    Skipped(IamPreflightSkipped),
    Checked(IamPreflightReport),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IamPreflightSkipped {
    pub reason: carina_core::provider_text::DiagnosticText,
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
    pub missing_actions: Vec<carina_core::provider_text::DiagnosticText>,
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
pub(crate) struct RequiredAction<A = RequiredPermission> {
    pub effect: EffectAddress,
    pub action: A,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimulationResult {
    denied_actions: BTreeSet<RequiredPermission>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentFallbackResult {
    allowed_actions: BTreeSet<RequiredPermission>,
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
    provider: &dyn HostProvider,
    schemas: &SchemaRegistry,
    strict: bool,
) -> Result<IamPreflightResult, crate::error::AppError> {
    let required = collect_required_actions(plan, provider, schemas)?;
    if required.is_empty() {
        return Ok(IamPreflightResult::Checked(IamPreflightReport {
            actor_arn: String::new(),
            method: IamCheckMethod::SimulatePrincipalPolicy,
            source_providers: plan_provider_names(plan),
            missing_by_effect: Vec::new(),
        }));
    }

    let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let sts_client = aws_sdk_sts::Client::new(&config);
    let actor_arn = match resolve_actor_arn(&sts_client).await {
        Ok(actor) => actor,
        Err(e) => {
            return Ok(IamPreflightResult::Skipped(IamPreflightSkipped {
                reason: format!(
                    "Warning: IAM preflight check skipped because AWS caller identity could not be resolved ({e})."
                ).into(),
            }));
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
                            !action_allowed_by_documents(*action, &fallback.allowed_actions)
                        })
                        .cloned()
                        .collect();
                    (IamCheckMethod::DocumentFallback, missing)
                }
                Err(e) => {
                    return Ok(IamPreflightResult::Skipped(IamPreflightSkipped {
                        reason: format!(
                            "Warning: IAM preflight check skipped for actor {} because IAM policy simulation was denied and IAM policies could not be read for fallback ({e}). \
                             The actor needs `iam:SimulatePrincipalPolicy` (with `Resource = {}`) OR `iam:GetRolePolicy` + `iam:ListAttachedRolePolicies` for the fallback path. \
                             Add the grant to enable --check-iam.",
                            actor_arn.as_str(),
                            actor_arn.as_str(),
                        ).into(),
                    }));
                }
            }
        }
        Err(SimulateError::Other(e)) => {
            return Ok(IamPreflightResult::Skipped(IamPreflightSkipped {
                reason: format!(
                    "Warning: IAM preflight check skipped because IAM policy simulation failed ({e})."
                ).into(),
            }));
        }
    };

    let report = IamPreflightReport {
        actor_arn: actor_arn.as_str().to_string(),
        method,
        source_providers: plan_provider_names(plan),
        missing_by_effect: group_missing_by_effect(&required, &missing_actions),
    };

    if strict && !report.missing_by_effect.is_empty() {
        return Ok(IamPreflightResult::Checked(report));
    }

    Ok(IamPreflightResult::Checked(report))
}

pub(crate) fn collect_required_actions(
    plan: &Plan,
    provider: &dyn HostProvider,
    schemas: &SchemaRegistry,
) -> Result<Vec<RequiredAction>, crate::error::AppError> {
    let mut required = Vec::new();
    for effect in plan.effects() {
        for (id, op) in effect_required_ops(effect, schemas) {
            let actions = provider.required_permissions(&id, op)?;
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
    Ok(required)
}

fn effect_required_ops(effect: &Effect, _schemas: &SchemaRegistry) -> Vec<(ResourceId, PlanOp)> {
    match effect {
        Effect::Read { resource } => vec![(resource.id.clone(), PlanOp::Read)],
        Effect::Create(resource) => vec![(resource.id.clone(), PlanOp::Create)],
        Effect::ResumeCreate { resource, .. } => vec![(resource.id.clone(), PlanOp::Create)],
        Effect::Update { to, .. } => vec![(to.id.clone(), PlanOp::Update)],
        Effect::Delete { id, .. } => vec![(id.clone().into_inner(), PlanOp::Delete)],
        Effect::Import { id, .. } => vec![(id.clone().into_inner(), PlanOp::Read)],
        Effect::DeferredCreate(payload) => {
            vec![(
                payload.template.template_resource_id().clone(),
                PlanOp::Create,
            )]
        }
        Effect::DeferredReplace(payload) => {
            let mut ops: Vec<_> = payload
                .deletes
                .iter()
                .map(|delete| (delete.id.clone().into_inner(), PlanOp::Delete))
                .collect();
            ops.push((
                payload.template.template_resource_id().clone(),
                PlanOp::Create,
            ));
            ops
        }
        Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::ReleaseListKeys { .. }
        | Effect::Wait { .. } => Vec::new(),
    }
}

fn effect_resource_ids(effect: &Effect) -> Vec<&ResourceId> {
    match effect {
        Effect::Read { resource } => vec![&resource.id],
        Effect::Create(resource) => vec![&resource.id],
        Effect::ResumeCreate { resource, .. } => vec![&resource.id],
        Effect::Update { to, .. } => vec![&to.id],
        Effect::Delete { id, .. } => vec![id.as_inner()],
        Effect::Import { id, .. } => vec![id.as_inner()],
        Effect::Remove { id, .. } => vec![id.as_inner()],
        Effect::ReleaseListKeys { id, .. } => vec![id.as_inner()],
        Effect::Move { from, to, .. } => vec![from.as_inner(), to.as_inner()],
        Effect::Wait { .. } => Vec::new(),
        Effect::DeferredCreate(payload) => vec![payload.id().as_inner()],
        Effect::DeferredReplace(payload) => {
            let mut ids = vec![payload.id().as_inner()];
            ids.extend(payload.deletes.iter().map(|delete| delete.id.as_inner()));
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

pub(crate) fn emit_warnings(
    color_mode: crate::output_style::ColorMode,
    result: &IamPreflightResult,
) {
    if let Some(warning) = format_warnings(result) {
        eprintln!("{}", warning.yellow(color_mode));
    }
}

pub(crate) fn print_warnings(
    color_mode: crate::output_style::ColorMode,
    result: &IamPreflightResult,
) {
    if let Some(warning) = format_warnings(result) {
        println!("{}", warning.yellow(color_mode));
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

fn unique_actions(required: &[RequiredAction]) -> BTreeSet<RequiredPermission> {
    required.iter().map(|entry| entry.action.clone()).collect()
}

async fn simulate(
    actor_arn: &PolicySourceArn,
    actions: &BTreeSet<RequiredPermission>,
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
            request = request.action_names(action.iam_action());
        }

        let output = request
            .send()
            .await
            .map_err(|e| classify_simulate_error(e.into_service_error()))?;

        for result in output.evaluation_results() {
            if result.eval_decision() != &PolicyEvaluationDecisionType::Allowed {
                denied_actions.extend(
                    actions
                        .iter()
                        .filter(|action| action.matches_iam_action(result.eval_action_name()))
                        .cloned(),
                );
            }
        }

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
    actions: &BTreeSet<RequiredPermission>,
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

trait PolicyAction: Ord + Clone {
    fn matches_policy_pattern(&self, pattern: &str) -> bool;
}
impl PolicyAction for RequiredPermission {
    fn matches_policy_pattern(&self, pattern: &str) -> bool {
        self.matches_policy_pattern(pattern)
    }
}
#[cfg(test)]
impl PolicyAction for String {
    fn matches_policy_pattern(&self, pattern: &str) -> bool {
        action_matches(pattern, self)
    }
}

fn collect_allowed_actions_from_policy<A: PolicyAction>(
    policy_document: &str,
    required_actions: &BTreeSet<A>,
    allowed_actions: &mut BTreeSet<A>,
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
                .any(|pattern| required.matches_policy_pattern(pattern))
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

#[cfg(test)]
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

fn action_allowed_by_documents<A: Ord>(action: &A, allowed_actions: &BTreeSet<A>) -> bool {
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

fn group_missing_by_effect<A: Ord + Clone + std::fmt::Display>(
    required: &[RequiredAction<A>],
    missing_actions: &BTreeSet<A>,
) -> Vec<MissingEffectActions> {
    let mut grouped: BTreeMap<EffectAddress, BTreeSet<A>> = BTreeMap::new();
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
            missing_actions: actions
                .into_iter()
                .map(|action| action.to_string().into())
                .collect(),
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
    use carina_core::provider::Provider;

    #[test]
    fn provider_text_permission_report_keeps_action_data_and_sanitizes_display() {
        let raw = format!("\x1b[2J\x07{}", "x".repeat(20 * 1024));
        let required = vec![RequiredAction {
            effect: EffectAddress {
                resource: "test.example".into(),
                op: PlanOp::Create,
            },
            action: raw.clone(),
        }];
        let report = group_missing_by_effect(&required, &BTreeSet::from([raw.clone()]));
        let displayed = report[0].missing_actions.join(", ");
        assert!(!displayed.contains(['\x1b', '\x07']));
        assert!(displayed.len() <= carina_core::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(required[0].action, raw);
    }
    use aws_sdk_iam::error::ErrorMetadata;
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::provider::{
        BoxFuture, CreateRequest, ProviderError, ProviderResult, ReadRequest, UpdateRequest,
    };
    use carina_core::resource::{
        ConcreteValue, DataSource, ResolvedDataSource, ResolvedResource, Resource, State, Value,
    };
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::collect_resolved([resource])
            .expect("fixture identity resolves")
            .remove(0)
    }

    fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
        ResolvedDataSource::collect_resolved([resource])
            .expect("fixture identity resolves")
            .remove(0)
    }

    struct PermissionProvider {
        fail: bool,
    }

    impl Provider for PermissionProvider {
        fn name(&self) -> &str {
            "permission-test"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Err(ProviderError::not_found(id.to_string())) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            Box::pin(async move { Err(attempt.not_created(ProviderError::internal("unused"))) })
        }

        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
            _attempt: carina_core::provider::UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn delete(
            &self,
            _attempt: carina_core::provider::DeleteAttempt,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unused")) })
        }

        fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
            if id.resource_type == "SecretPermission" {
                return Ok(vec!["iam:GetRole\npermission-private-4305".into()]);
            }
            if id.resource_type == "SecretValidPermission" {
                return Ok(vec!["iam:PermissionPrivate4305".into()]);
            }
            if id.resource_type == "AwsccGeneratedPermissions" {
                // Generated s3/bucket.rs:957 and logs/log_group.rs:144,170.
                return Ok(vec![
                    "S3:GetBucketWebsite".into(),
                    "s3:REST.PUT.OBJECT".into(),
                    "s3:REST.PUT.OBJECT".into(),
                ]);
            }
            if self.fail {
                return Err(ProviderError::internal(
                    "intentional permission lookup failure",
                ));
            }
            Ok(vec![format!(
                "test:{}0{}",
                plan_op_label(op),
                id.resource_type.replace('.', "Dot")
            )])
        }
    }

    #[test]
    fn review_round7_invalid_permission_never_reaches_preflight_output() {
        const CHILD: &str = "CARINA_4305_ROUND7_IAM_CHILD";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::iam_preflight::tests::review_round7_invalid_permission_never_reaches_preflight_output", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.status.success(), "preflight child failed: {text}");
            assert!(
                !text.contains("permission-private-4305"),
                "preflight stdout/stderr leaked: {text}"
            );
            assert!(text.contains("index 0"));
            return;
        }
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::with_provider(
            "awscc",
            "SecretPermission",
            "main",
            None,
        ))));
        let result = collect_required_actions(
            &plan,
            &crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: false })),
            &SchemaRegistry::new(),
        );
        let output = match result {
            Err(error) => {
                if let crate::error::AppError::Provider(host) = &error {
                    assert!(
                        host.detail()
                            .cause()
                            .unwrap()
                            .is::<carina_core::provider_text::InvalidRequiredPermission>()
                    );
                } else {
                    panic!("permission rejection lost its typed provider error");
                }
                error.to_string()
            }
            Ok(required) => format_warnings(&IamPreflightResult::Checked(IamPreflightReport {
                actor_arn: "fixture actor".into(),
                method: IamCheckMethod::SimulatePrincipalPolicy,
                source_providers: vec!["awscc".into()],
                missing_by_effect: group_missing_by_effect(&required, &unique_actions(&required)),
            }))
            .unwrap(),
        };
        assert!(
            !output.contains("permission-private-4305"),
            "guest permission leaked: {output}"
        );
        assert!(
            output.contains("index 0"),
            "missing typed rejection: {output}"
        );
        println!("{output}");
    }

    #[test]
    fn review_round7_valid_permission_masks_display_and_keeps_iam_request_action() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::with_provider(
            "awscc",
            "SecretValidPermission",
            "main",
            None,
        ))));
        let provider = crate::provider_text_fixtures::initialized_with_context(
            Box::new(PermissionProvider { fail: false }),
            &crate::provider_text_fixtures::context(&["PermissionPrivate4305"]),
        );
        let required =
            collect_required_actions(&plan, provider.provider(), &SchemaRegistry::new()).unwrap();
        let output = format_warnings(&IamPreflightResult::Checked(IamPreflightReport {
            actor_arn: "fixture actor".into(),
            method: IamCheckMethod::SimulatePrincipalPolicy,
            source_providers: vec!["awscc".into()],
            missing_by_effect: group_missing_by_effect(&required, &unique_actions(&required)),
        }))
        .unwrap();
        assert!(!output.contains("PermissionPrivate4305"));
        assert!(output.contains("iam:(secret)"));
        let actor =
            PolicySourceArn::from_caller_identity_arn("arn:aws:iam::123456789012:role/deploy")
                .unwrap();
        let input = build_simulate_input_for_test(
            &actor,
            &BTreeSet::from([required[0].action.iam_action().to_owned()]),
            None,
        );
        assert_eq!(input.action_names(), &["iam:PermissionPrivate4305"]);
    }

    #[tokio::test]
    async fn review_round8_awscc_permissions_pass_boundary_and_keep_iam_simulate_action_names() {
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct CaptureActions(Arc<Mutex<Vec<String>>>);
        impl aws_sdk_iam::config::Intercept for CaptureActions {
            fn name(&self) -> &'static str {
                "CaptureActions"
            }
            fn read_before_execution(
                &self,
                context: &aws_sdk_iam::config::interceptors::BeforeSerializationInterceptorContextRef<
                    '_,
                >,
                _cfg: &mut aws_sdk_iam::config::ConfigBag,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let input = context
                    .input()
                    .downcast_ref::<aws_sdk_iam::operation::simulate_principal_policy::SimulatePrincipalPolicyInput>()
                    .expect("the production simulator must send a SimulatePrincipalPolicy input");
                *self.0.lock().unwrap() = input.action_names().to_vec();
                Err(std::io::Error::other("captured IAM simulate input").into())
            }
        }

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::with_provider(
            "awscc",
            "AwsccGeneratedPermissions",
            "main",
            None,
        ))));
        let provider =
            crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: false }));
        let required = collect_required_actions(&plan, &provider, &SchemaRegistry::new())
            .expect("all three generated AWSCC permissions must pass the real provider boundary");
        assert_eq!(
            required
                .iter()
                .map(|entry| entry.action.iam_action())
                .collect::<Vec<_>>(),
            [
                "S3:GetBucketWebsite",
                "s3:REST.PUT.OBJECT",
                "s3:REST.PUT.OBJECT"
            ]
        );

        let captured = Arc::new(Mutex::new(Vec::new()));
        let client = aws_sdk_iam::Client::from_conf(
            aws_sdk_iam::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_iam::config::Region::new("us-east-1"))
                .http_client(winterbaume_core::MockAws::builder().build().http_client())
                .interceptor(CaptureActions(Arc::clone(&captured)))
                .build(),
        );
        let actor =
            PolicySourceArn::from_caller_identity_arn("arn:aws:iam::123456789012:role/deploy")
                .unwrap();
        // Capture the production request before network I/O; de-duplication
        // keeps the two distinct spellings from the three provider entries.
        assert!(
            simulate(&actor, &unique_actions(&required), &client)
                .await
                .is_err()
        );
        assert_eq!(
            *captured.lock().unwrap(),
            ["S3:GetBucketWebsite", "s3:REST.PUT.OBJECT"]
        );
    }

    fn collect_fixture_required_actions(
        plan: &Plan,
        provider: &dyn HostProvider,
        schemas: &SchemaRegistry,
    ) -> Result<Vec<RequiredAction<String>>, crate::error::AppError> {
        // Preserve the routing fixtures' historical labels after passing
        // grammar-valid identifiers through the production boundary.
        super::collect_required_actions(plan, provider, schemas).map(|actions| {
            actions
                .into_iter()
                .map(|entry| {
                    let (op, resource) = entry
                        .action
                        .iam_action()
                        .strip_prefix("test:")
                        .unwrap()
                        .split_once('0')
                        .unwrap();
                    RequiredAction {
                        effect: entry.effect,
                        action: format!("test:{op}:{}", resource.replace("Dot", ".")),
                    }
                })
                .collect()
        })
    }

    #[test]
    fn collect_required_actions_maps_effect_variants() {
        let mut plan = Plan::new();
        let read = DataSource::with_provider("awscc", "identity.User", "me", None);
        let create = Resource::with_provider("awscc", "ec2.Vpc", "new", None);
        let update_id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "old", None);
        let update_to = Resource::with_provider("awscc", "ec2.Subnet", "old", None);
        let replace_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "old", None);
        let replace_to = Resource::with_provider("awscc", "s3.Bucket", "new", None);
        let delete_id = ResourceId::with_provider_identity("awscc", "iam.Role", "old", None);
        let import_id = ResourceId::with_provider_identity("awscc", "logs.Group", "existing", None);

        plan.add(Effect::Read {
            resource: resolved_data_source(read),
        });
        plan.add(Effect::Create(resolved(create)));
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("subnet-old"),
            from: Box::new(
                (State::<carina_core::resource::NoPlaceholder>::existing(
                    update_id.clone(),
                    Default::default(),
                )
                .with_identifier("subnet-old"))
                .to_snapshot(),
            ),
            to: resolved(update_to),
            changed_attributes: vec!["cidr".to_string()],
        });
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(replace_id.clone()),
            identifier: crate::fixture_applied_identifier("bucket-old"),
            generation: carina_core::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        });
        plan.add(Effect::Create(resolved(replace_to)));
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(delete_id.clone()),
            identifier: crate::fixture_applied_identifier("role-id"),
            generation: carina_core::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        });
        plan.add(Effect::Import {
            id: carina_core::resource::ResolvedResourceId::new(import_id),
            identifier: Value::Concrete(ConcreteValue::String("group".into())),
        });
        plan.add(Effect::Remove {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
                "awscc",
                "skip.Remove",
                "x",
                None,
            )),
        });
        plan.add(Effect::Move {
            from: carina_core::resource::ResolvedResourceId::new(
                ResourceId::with_provider_identity("awscc", "skip.Move", "x", None),
            ),
            to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
                "awscc",
                "skip.Move",
                "y",
                None,
            )),
            origin: None,
        });

        let entries = collect_fixture_required_actions(
            &plan,
            &crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: false })),
            &SchemaRegistry::new(),
        )
        .expect("permission lookup should succeed");
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
        let template = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            carina_core::parser::DeferredForTarget::bound(
                carina_core::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::resource_ref("cert", "domain_validation_options", Vec::new())
                .resolve_root_for_test(),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );
        plan.add(Effect::DeferredCreate(crate::test_support::compiled_loop(
            Box::new(template),
        )));

        let entries = collect_fixture_required_actions(
            &plan,
            &crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: false })),
            &SchemaRegistry::new(),
        )
        .expect("permission lookup should succeed");
        let actions: Vec<_> = entries.into_iter().map(|entry| entry.action).collect();

        assert_eq!(actions, vec!["test:create:route53.RecordSet"]);
    }

    #[test]
    fn collect_required_actions_accepts_deferred_template_with_ignore_changes() {
        let mut template_resource =
            Resource::with_provider("aws", "route53.RecordSet", "validation_records", None)
                .with_attribute(
                    "comment",
                    Value::Concrete(ConcreteValue::String("managed-elsewhere".into())),
                );
        template_resource.directives.ignore_changes = vec![
            serde_json::from_value(serde_json::json!({
                "segments": [{ "kind": "attribute", "value": "comment" }]
            }))
            .expect("valid ignore path"),
        ];
        let mut plan = Plan::new();
        let template = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            carina_core::parser::DeferredForTarget::bound(
                carina_core::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::resource_ref("cert", "domain_validation_options", Vec::new())
                .resolve_root_for_test(),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );
        plan.add(Effect::DeferredCreate(crate::test_support::compiled_loop(
            Box::new(template),
        )));

        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            ResourceSchema::new("route53.RecordSet")
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .validate()
                .expect("valid IAM preflight schema"),
        );
        let entries = collect_fixture_required_actions(
            &plan,
            &crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: false })),
            &schemas,
        )
        .expect("permission lookup should resolve authored ignore paths");

        assert_eq!(entries[0].action, "test:create:route53.RecordSet");
    }

    #[test]
    fn collect_required_actions_propagates_provider_failure() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::with_provider(
            "awscc", "ec2.Vpc", "main", None,
        ))));

        let error = collect_required_actions(
            &plan,
            &crate::provider_text_fixtures::boundary(Box::new(PermissionProvider { fail: true })),
            &SchemaRegistry::new(),
        )
        .expect_err("permission lookup failures must abort IAM preflight");

        assert!(
            error
                .to_string()
                .contains("intentional permission lookup failure"),
            "the provider error must be preserved: {error}"
        );
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
                    "iam:CreateServiceLinkedRole".into(),
                    "ec2:DescribeInternetGateways".into(),
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
