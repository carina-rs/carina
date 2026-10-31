//! Parser-template nodes for `if` expressions whose condition depends only on
//! module arguments.
//!
//! These nodes are resolved while a module call substitutes its arguments.
//! They must never survive into the expanded root graph, a saved plan, or
//! state.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::resource::{
    DataSource, ModuleSource, ParserPlaceholder, PlaceholderKind, Resource, Value,
};

use super::{File, ModuleCall, ParsedDeferredForPhase, ParsedExportParam};

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredIfCondition<P: PlaceholderKind = ParserPlaceholder> {
    pub(crate) value: Value<P>,
    pub(crate) source_file: PathBuf,
    pub(crate) source_line: usize,
    pub(crate) source_expression: String,
}

impl DeferredIfCondition<ParserPlaceholder> {
    #[cfg(test)]
    pub(crate) fn new(
        value: Value<ParserPlaceholder>,
        source_file: PathBuf,
        source_line: usize,
    ) -> Self {
        let source_expression = match &value {
            Value::Deferred(crate::resource::DeferredValue::Placeholder(
                ParserPlaceholder::ModuleArgument { name, diagnostic },
            )) => diagnostic.diagnostic_name().unwrap_or(name).to_string(),
            _ => crate::value::format_value(&value),
        };
        Self::new_with_source_expression(value, source_file, source_line, source_expression)
    }
}

impl<P: PlaceholderKind> DeferredIfCondition<P> {
    pub(crate) fn new_with_source_expression(
        value: Value<P>,
        source_file: PathBuf,
        source_line: usize,
        source_expression: String,
    ) -> Self {
        Self {
            value,
            source_file,
            source_line,
            source_expression,
        }
    }

    pub fn value(&self) -> &Value<P> {
        &self.value
    }

    pub fn source_file(&self) -> &Path {
        &self.source_file
    }

    pub fn source_line(&self) -> usize {
        self.source_line
    }

    pub fn source_expression(&self) -> &str {
        &self.source_expression
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredValueIf<P: PlaceholderKind = ParserPlaceholder> {
    pub(crate) condition: DeferredIfCondition<P>,
    pub(crate) then_value: Value<P>,
    pub(crate) else_value: Value<P>,
}

impl<P: PlaceholderKind> DeferredValueIf<P> {
    pub(crate) fn new(
        condition: DeferredIfCondition<P>,
        then_value: Value<P>,
        else_value: Value<P>,
    ) -> Self {
        Self {
            condition,
            then_value,
            else_value,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<P> {
        &self.condition
    }

    pub fn then_value(&self) -> &Value<P> {
        &self.then_value
    }

    pub fn else_value(&self) -> &Value<P> {
        &self.else_value
    }

    pub fn values(&self) -> [&Value<P>; 3] {
        [self.condition.value(), &self.then_value, &self.else_value]
    }

    pub(crate) fn map(&self, mut map: impl FnMut(&Value<P>) -> Value<P>) -> Self {
        self.try_map::<std::convert::Infallible>(|value| Ok(map(value)))
            .expect("an infallible deferred-if map cannot fail")
    }

    pub(crate) fn try_map<E>(
        &self,
        mut map: impl FnMut(&Value<P>) -> Result<Value<P>, E>,
    ) -> Result<Self, E> {
        Ok(Self::new(
            DeferredIfCondition::new_with_source_expression(
                map(self.condition.value())?,
                self.condition.source_file.clone(),
                self.condition.source_line,
                self.condition.source_expression.clone(),
            ),
            map(&self.then_value)?,
            map(&self.else_value)?,
        ))
    }

    pub(crate) fn try_map_stage<Q: PlaceholderKind, E>(
        self,
        mut map: impl FnMut(Value<P>) -> Result<Value<Q>, E>,
    ) -> Result<DeferredValueIf<Q>, E> {
        Ok(DeferredValueIf::new(
            DeferredIfCondition::new_with_source_expression(
                map(self.condition.value)?,
                self.condition.source_file,
                self.condition.source_line,
                self.condition.source_expression,
            ),
            map(self.then_value)?,
            map(self.else_value)?,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct DeferredStructuralIf<P: PlaceholderKind = ParserPlaceholder> {
    pub(crate) condition: DeferredIfCondition<P>,
    pub(crate) then_branch: DeferredIfBody<P>,
    pub(crate) else_branch: Option<DeferredIfBody<P>>,
}

impl<P: PlaceholderKind> DeferredStructuralIf<P> {
    pub(crate) fn new(
        condition: DeferredIfCondition<P>,
        then_branch: DeferredIfBody<P>,
        else_branch: Option<DeferredIfBody<P>>,
    ) -> Self {
        Self {
            condition,
            then_branch,
            else_branch,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<P> {
        &self.condition
    }

    pub fn then_branch(&self) -> &DeferredIfBody<P> {
        &self.then_branch
    }

    pub fn else_branch(&self) -> Option<&DeferredIfBody<P>> {
        self.else_branch.as_ref()
    }

    pub(crate) fn branches(&self) -> impl Iterator<Item = &DeferredIfBody<P>> {
        std::iter::once(&self.then_branch).chain(self.else_branch.iter())
    }

    pub(crate) fn branches_mut(&mut self) -> impl Iterator<Item = &mut DeferredIfBody<P>> {
        std::iter::once(&mut self.then_branch).chain(self.else_branch.iter_mut())
    }
}

#[derive(Debug, Clone)]
pub enum DeferredIfBody<P: PlaceholderKind = ParserPlaceholder> {
    Resource(Box<Resource<ModuleSource, P>>),
    DataSource(Box<DataSource<ModuleSource, P>>),
    ModuleCall(ModuleCall<P>),
    /// Resolver-prepared nested call contribution. Parsing only constructs
    /// `ModuleCall`; nested-module resolution replaces it with this form
    /// while the callee's binding/type scope is still available.
    ExpandedModuleCall(Arc<File<ParsedExportParam<P>, P, P, ParsedDeferredForPhase<P>>>),
}

impl<P: PlaceholderKind> DeferredIfBody<P> {
    pub(crate) fn binding(&self) -> Option<&str> {
        match self {
            Self::Resource(resource) => resource.binding.as_deref(),
            Self::DataSource(data_source) => data_source.binding.as_deref(),
            Self::ModuleCall(call) => call.binding_name.as_deref(),
            Self::ExpandedModuleCall(contribution) => contribution
                .compositions
                .iter()
                .find_map(|composition| composition.binding.as_deref()),
        }
    }
}
