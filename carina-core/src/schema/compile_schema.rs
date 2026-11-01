//! Consume draft schema trees at admission. Ref targets are compiled once in
//! the definition map, so cycles never require expansion or a traversal budget.

use super::*;

/// Visit every sibling before combining the results. Collecting the mapped
/// iterator directly into Option would skip siblings after the first failure.
pub(super) fn compile_all<T, U, C: FromIterator<U>>(
    items: impl IntoIterator<Item = T>,
    compile: impl FnMut(T) -> Option<U>,
) -> Option<C> {
    let results: Vec<_> = items.into_iter().map(compile).collect();
    results.into_iter().collect()
}

impl PatternAttributeType<Draft> {
    pub(super) fn compile_patterns(
        self,
        location: SchemaValidationIssueLocation,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<CompiledAttributeType> {
        let kind = match self.kind {
            AttrTypeKind::String {
                binding,
                pattern,
                length,
                to_dsl,
            } => AttrTypeKind::String {
                binding,
                pattern: match pattern {
                    Some(pattern) => match CompiledPattern::try_new(&pattern) {
                        Ok(pattern) => Some(pattern),
                        Err(error) => {
                            issues.push(SchemaDefinitionIssue {
                                path: location.to_string(),
                                location,
                                message: error.to_string(),
                                pattern_error: Some(error),
                            });
                            return None;
                        }
                    },
                    None => None,
                },
                length,
                to_dsl,
            },
            AttrTypeKind::ResourceHandle {
                provider,
                resource_type,
            } => AttrTypeKind::ResourceHandle {
                provider,
                resource_type,
            },
            AttrTypeKind::StringLiteralSet { values } => AttrTypeKind::StringLiteralSet { values },
            AttrTypeKind::Int { binding, range } => AttrTypeKind::Int { binding, range },
            AttrTypeKind::Float { binding, range } => AttrTypeKind::Float { binding, range },
            AttrTypeKind::Bool => AttrTypeKind::Bool,
            AttrTypeKind::Duration => AttrTypeKind::Duration,
            AttrTypeKind::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate,
                to_dsl,
            } => AttrTypeKind::Enum {
                identity,
                base: Box::new(base.compile_patterns(location.with_suffix(".<base>"), issues)?),
                values,
                dsl_aliases,
                validate,
                to_dsl,
            },
            AttrTypeKind::List {
                element_type,
                ordered,
                length,
            } => AttrTypeKind::List {
                element_type: Box::new(
                    element_type.compile_patterns(location.with_suffix("[]"), issues)?,
                ),
                ordered,
                length,
            },
            AttrTypeKind::Map { key, value } => {
                let key = key.compile_patterns(location.with_suffix(".<key>"), issues);
                let value = value.compile_patterns(location.with_suffix(".<value>"), issues);
                AttrTypeKind::Map {
                    key: Box::new(key?),
                    value: Box::new(value?),
                }
            }
            AttrTypeKind::Struct {
                name,
                fields,
                field_groups,
            } => AttrTypeKind::Struct {
                name,
                fields: compile_all(fields, |field| {
                    let field_location = location.with_suffix(format_args!(".{}", field.name));
                    field.compile_patterns(field_location, issues)
                })?,
                field_groups,
            },
            AttrTypeKind::Union(members) => {
                AttrTypeKind::Union(UnionMemberTypes::new(compile_all(
                    members.into_raw_vec().into_iter().enumerate(),
                    |(index, member)| {
                        member.compile_patterns(
                            location.with_suffix(format_args!(".<union:{index}>")),
                            issues,
                        )
                    },
                )?))
            }
            AttrTypeKind::Ref(name) => AttrTypeKind::Ref(name),
        };
        Some(PatternAttributeType { kind })
    }
}

impl PatternStructField<Draft> {
    fn compile_patterns(
        self,
        location: SchemaValidationIssueLocation,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<CompiledStructField> {
        let Self {
            name,
            field_type,
            declared_input_mode,
            description,
            provider_name,
            block_name,
            deferred_populate,
            write_only,
            create_only,
            identity_mode,
        } = self;
        Some(PatternStructField {
            name,
            field_type: field_type.compile_patterns(location, issues)?,
            declared_input_mode,
            description,
            provider_name,
            block_name,
            deferred_populate,
            write_only,
            create_only,
            identity_mode,
        })
    }
}

impl PatternAttributeSchema<Draft> {
    fn compile_patterns(
        self,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<CompiledAttributeSchema> {
        let Self {
            name,
            attr_type,
            input_mode,
            default,
            description,
            completions,
            provider_name,
            create_only,
            removable,
            block_name,
            write_only,
            keyed_partial_update,
            identity_mode,
            deferred_populate,
        } = self;
        let attr_type = attr_type.compile_patterns(
            SchemaValidationIssueLocation::Attribute(name.clone()),
            issues,
        )?;
        Some(PatternAttributeSchema {
            name,
            attr_type,
            input_mode,
            default,
            description,
            completions,
            provider_name,
            create_only,
            removable,
            block_name,
            write_only,
            keyed_partial_update,
            identity_mode,
            deferred_populate,
        })
    }
}

impl PatternResourceSchema<Draft> {
    pub(super) fn compile_patterns(
        self,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<CompiledResourceSchema> {
        let Self {
            resource_type,
            attributes,
            duplicate_attributes,
            description,
            validation,
            kind,
            unique_name,
            operation_config,
            field_groups,
            default_prevent_destroy,
            default_wait_timeout,
            default_wait_interval,
            defs,
        } = self;
        let attributes = compile_all(attributes, |(name, attr)| {
            attr.compile_patterns(issues).map(|attr| (name, attr))
        });
        let defs = compile_all(defs, |(name, ty)| {
            let location = SchemaValidationIssueLocation::Definition(name.clone());
            ty.compile_patterns(location, issues).map(|ty| (name, ty))
        });
        Some(PatternResourceSchema {
            resource_type,
            attributes: attributes?,
            duplicate_attributes,
            description,
            validation,
            kind,
            unique_name,
            operation_config,
            field_groups,
            default_prevent_destroy,
            default_wait_timeout,
            default_wait_interval,
            defs: defs?,
        })
    }
}
