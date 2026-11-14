//! The existing source-type admission, parameterized by its judgement owner.
use super::*;
use crate::schema::CompiledAttributeType;

pub(super) enum StaticSourceData<'a> {
    Borrowed(&'a JudgedExprData),
    Converted(InferredExpr),
    WalkOwned,
}
impl StaticSourceData<'_> {
    pub(super) fn data(&self) -> Option<&JudgedExprData> {
        match self {
            Self::Borrowed(data) => Some(data),
            Self::Converted(data) => Some(&data.data),
            Self::WalkOwned => None,
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum StaticChild<'a> {
    Union(usize),
    List,
    Map,
    Field(&'a str),
}
pub(super) trait StaticSourceMode<'d>: Sized {
    type Error: From<InferenceError> + From<MissingEvidence>;
    fn enter<'a>(
        &mut self,
        data: &'a JudgedExprData,
        sink: &TypeExpr,
        context: AdmissionContext,
        declaration: Option<&CompiledAttributeType>,
        config: &super::super::super::ProviderContext,
    ) -> Result<StaticSourceData<'a>, Self::Error>;
    fn child(
        &self,
        declaration: Option<&'d CompiledAttributeType>,
        child: StaticChild<'_>,
    ) -> Result<Option<&'d CompiledAttributeType>, Self::Error>;
    fn value_check(&self) -> ExpressionAdmission;
    fn probe(&self) -> Self;
    fn commit(&mut self, probe: Self);
    fn failed_union(
        &mut self,
        probes: Vec<(Self, Self::Error)>,
        fallback: InferenceError,
    ) -> Self::Error;
}
pub(super) struct OrdinarySourceMode(pub(super) NoConversion);
impl<'d> StaticSourceMode<'d> for OrdinarySourceMode {
    type Error = InferenceError;
    fn enter<'a>(
        &mut self,
        data: &'a JudgedExprData,
        _sink: &TypeExpr,
        _context: AdmissionContext,
        _declaration: Option<&CompiledAttributeType>,
        _config: &super::super::super::ProviderContext,
    ) -> Result<StaticSourceData<'a>, Self::Error> {
        Ok(StaticSourceData::Borrowed(data))
    }
    fn child(
        &self,
        _declaration: Option<&'d CompiledAttributeType>,
        _child: StaticChild<'_>,
    ) -> Result<Option<&'d CompiledAttributeType>, Self::Error> {
        Ok(None)
    }
    fn value_check(&self) -> ExpressionAdmission {
        ordinary_value_check(self.0)
    }
    fn probe(&self) -> Self {
        Self(self.0)
    }
    fn commit(&mut self, _probe: Self) {}
    fn failed_union(
        &mut self,
        _probes: Vec<(Self, Self::Error)>,
        fallback: InferenceError,
    ) -> Self::Error {
        fallback
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum JudgeSourceError {
    Ordinary(InferenceError),
    Conversion,
}
impl From<InferenceError> for JudgeSourceError {
    fn from(error: InferenceError) -> Self {
        Self::Ordinary(error)
    }
}
impl From<MissingEvidence> for JudgeSourceError {
    fn from(error: MissingEvidence) -> Self {
        Self::Ordinary(error.into())
    }
}
struct JudgeSourceMode<'d> {
    defs: &'d std::collections::BTreeMap<String, CompiledAttributeType>,
    records: Vec<(
        Arc<conversion::ConversionTypeJudgement>,
        conversion::StaticVerdict,
    )>,
}
impl<'d> StaticSourceMode<'d> for JudgeSourceMode<'d> {
    type Error = JudgeSourceError;
    fn enter<'a>(
        &mut self,
        data: &'a JudgedExprData,
        sink: &TypeExpr,
        context: AdmissionContext,
        declaration: Option<&CompiledAttributeType>,
        config: &super::super::super::ProviderContext,
    ) -> Result<StaticSourceData<'a>, Self::Error> {
        let Some(converted) = &data.converted else {
            return Ok(StaticSourceData::Borrowed(data));
        };
        let declaration = declaration
            .ok_or_else(|| MissingEvidence::new("static conversion destination declaration"))?;
        // This use owns target/destination legality even when its source's
        // ordinary twin delegates to the value walk (including read-only runs).
        let mut verdict = converted.compute_destination(declaration, self.defs, config);
        if verdict.outcome().is_err() {
            self.records.push((converted.clone(), verdict));
            return Err(JudgeSourceError::Conversion);
        }
        let twin = converted.source().for_sink(sink, config);
        let context = if converted.is_bare() {
            AdmissionContext::Assignment
        } else {
            context
        };
        // Match main's precheck: an unavailable or still value-dependent
        // source supplies no static source type. Its destination walk owns it.
        let awaiting_source = match &twin.judgement {
            ExprJudgement::ValueDependent { shape: None, .. }
            | ExprJudgement::AuthorityUnavailable(_) => true,
            ExprJudgement::ValueDependent { shape: Some(_), .. }
            | ExprJudgement::Failed { .. }
            | ExprJudgement::Rejected { .. }
            | ExprJudgement::Typed { .. }
            | ExprJudgement::Declaration => false,
        };
        if awaiting_source
            || converted.source().structural_source
            || (context == AdmissionContext::Resource
                && matches!(twin.kind(), Some(InferredKind::Reference(_)))
                && twin.expression.check_owner() == ExpressionCheckOwner::BindingIndex)
            || (context == AdmissionContext::StructValue
                && twin.concrete()
                && !twin.requires_declared_assignment())
        {
            self.records.push((converted.clone(), verdict));
            // A handle still needs precisely the twin's ordinary type and
            // bare-handle diagnostics. Only conversion source checking abstains.
            return Ok(if converted.source().structural_source {
                StaticSourceData::Borrowed(&converted.source().data)
            } else {
                StaticSourceData::WalkOwned
            });
        }
        verdict = verdict.with_source(
            converted.source(),
            &conversion::ConversionContext {
                catalog: config,
                reference: crate::validation::admission::ReferenceSource::Unchecked,
            },
            conversion::SourceKnowledge::StaticOnly,
        );
        let outcome = verdict.outcome();
        let walk_owned = verdict.source_is_walk_owned();
        self.records.push((converted.clone(), verdict));
        match outcome {
            Ok(_) if walk_owned => Ok(StaticSourceData::WalkOwned),
            Ok(ty) => {
                let mut projected = declared(Ok(ty), data.concrete());
                projected.data_mut().diagnostic_scope = data.diagnostic_scope;
                projected.data_mut().authority = data.authority.clone();
                Ok(StaticSourceData::Converted(projected))
            }
            Err(_error) => Err(JudgeSourceError::Conversion),
        }
    }
    fn child(
        &self,
        declaration: Option<&'d CompiledAttributeType>,
        child: StaticChild<'_>,
    ) -> Result<Option<&'d CompiledAttributeType>, Self::Error> {
        use crate::schema::AttrTypeKind;
        let declaration =
            declaration.ok_or_else(|| MissingEvidence::new("static source child destination"))?;
        let declaration =
            crate::schema::value_validation::resolve_assignment_declaration(declaration, self.defs)
                .map_err(|error| InferenceError::SchemaResolution {
                    error: Box::new(error),
                })?;
        let selected = match child {
            StaticChild::Union(index) => {
                crate::schema::union_members_with_defs(declaration, self.defs).and_then(|members| {
                    members
                        .iter()
                        .nth(index)
                        .flatten()
                        .map(|member| member.as_attr())
                })
            }
            StaticChild::List => {
                if let AttrTypeKind::List { element_type, .. } = declaration.kind() {
                    Some(element_type.as_ref())
                } else {
                    None
                }
            }
            StaticChild::Map => {
                if let AttrTypeKind::Map { value, .. } = declaration.kind() {
                    Some(value.as_ref())
                } else {
                    None
                }
            }
            StaticChild::Field(name) => {
                if let AttrTypeKind::Struct { fields, .. } = declaration.kind() {
                    fields
                        .iter()
                        .find(|field| field.name == name)
                        .map(|field| &field.field_type)
                } else {
                    None
                }
            }
        };
        selected
            .map(Some)
            .ok_or_else(|| MissingEvidence::new("static source destination child").into())
    }
    // This is a static source-type check, not permission for an ordinary value
    // callback: the authored value still carries its conversion at the cursor.
    fn value_check(&self) -> ExpressionAdmission {
        ExpressionAdmission::ValueCheck
    }
    fn probe(&self) -> Self {
        Self {
            defs: self.defs,
            records: Vec::new(),
        }
    }
    fn commit(&mut self, probe: Self) {
        self.records.extend(probe.records);
    }
    fn failed_union(
        &mut self,
        probes: Vec<(Self, Self::Error)>,
        fallback: InferenceError,
    ) -> Self::Error {
        for (probe, error) in probes {
            match error {
                JudgeSourceError::Conversion => {
                    self.commit(probe);
                    return JudgeSourceError::Conversion;
                }
                JudgeSourceError::Ordinary(_) => {}
            }
        }
        JudgeSourceError::Ordinary(fallback)
    }
}

pub(super) enum ResourceSourceAdmission {
    Ordinary(Result<ExpressionAdmission, InferenceError>),
    WalkOwned(Result<ExpressionAdmission, InferenceError>),
    JudgedStatically(Result<ExpressionAdmission, JudgeSourceError>),
}
pub(super) fn admit_judged_resource_source(
    expression: &JudgedExpr,
    sink: &TypeExpr,
    declaration: &CompiledAttributeType,
    defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
    config: &super::super::super::ProviderContext,
    findings: &mut conversion::flow::PlacementFindings,
    bare: bool,
) -> ResourceSourceAdmission {
    if let Some(proof) = expression.no_conversion() {
        return ResourceSourceAdmission::Ordinary(admit_ordinary_with_context(
            OrdinaryJudgedExpr {
                data: &expression.data,
                proof,
            },
            sink,
            AdmissionContext::Resource,
            config,
        ));
    }
    let mut mode = JudgeSourceMode {
        defs,
        records: Vec::new(),
    };
    let result = admit_source_data(
        &expression.data,
        sink,
        if bare {
            AdmissionContext::Assignment
        } else {
            AdmissionContext::Resource
        },
        config,
        &mut mode,
        Some(declaration),
    );
    let mut checked_source = false;
    for (source, verdict) in mode.records {
        checked_source |= !verdict.source_is_walk_owned() || verdict.outcome().is_err();
        findings.append_static(source.record(verdict));
    }
    if checked_source {
        ResourceSourceAdmission::JudgedStatically(result)
    } else {
        ResourceSourceAdmission::WalkOwned(match result {
            Ok(admission) => Ok(admission),
            Err(JudgeSourceError::Ordinary(error)) => Err(error),
            Err(JudgeSourceError::Conversion) => {
                Err(MissingEvidence::new("static conversion verdict evidence").into())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn r16_static_sink_resolution_keeps_the_typed_error_source() {
        let expression = JudgedExpr::build(
            declared(
                Ok(TypeExpr::DottedUnresolved(
                    crate::parser::ResourceTypePath::new("absent", "Type"),
                )),
                true,
            )
            .data,
            conversion::flow::JudgementEdges::Leaf,
        );
        let mut config = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        config.custom_types =
            crate::parser::ProviderCustomTypes::loaded(Default::default(), Box::new(|_, _| Ok(())));
        let error = admit_ordinary_with_context(
            expression.ordinary().unwrap(),
            &TypeExpr::String,
            AdmissionContext::Assignment,
            &config,
        )
        .unwrap_err();
        assert!(
            error
                .source()
                .and_then(
                    |cause| cause.downcast_ref::<crate::static_type::StaticTypeResolutionError>()
                )
                .is_some(),
            "{error:?}"
        );
    }

    #[test]
    fn r16_static_child_resolution_keeps_the_typed_error_source() {
        let defs = Default::default();
        let mode = JudgeSourceMode {
            defs: &defs,
            records: Vec::new(),
        };
        let declaration = crate::schema::CompiledAttributeType::ref_("absent");
        let error = mode
            .child(Some(&declaration), StaticChild::List)
            .unwrap_err();
        match error {
            JudgeSourceError::Ordinary(error) => assert!(
                error
                    .source()
                    .and_then(|cause| cause.downcast_ref::<Box<crate::schema::TypeError>>())
                    .is_some(),
                "{error:?}"
            ),
            JudgeSourceError::Conversion => {
                panic!("ordinary schema lookup must not be conversion finding")
            }
        }
    }
}
