//! Parser-template nodes for `if` expressions whose condition depends only on
//! module arguments.
//!
//! These nodes are resolved while a module call substitutes its arguments.
//! They must never survive into the expanded root graph, a saved plan, or
//! state.

use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, DataSource, ModuleSource, RelativeBindingName,
    Resource, Value,
};
use std::path::{Path, PathBuf};

use super::ModuleCall;

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredIfCondition<B: BindingAddress = RelativeBindingName> {
    pub(crate) value: Value<B>,
    pub(crate) source_file: PathBuf,
    pub(crate) source_line: usize,
    pub(crate) source_expression: String,
}

impl<B: BindingAddress> DeferredIfCondition<B> {
    #[cfg(test)]
    pub(crate) fn new(value: Value<B>, source_file: PathBuf, source_line: usize) -> Self {
        let source_expression = if let Value::Deferred(crate::resource::DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument {
                name,
                source_line: _,
                diagnostic_name,
            },
        )) = &value
        {
            diagnostic_name.as_deref().unwrap_or(name).to_string()
        } else {
            crate::value::format_value(&value)
        };
        Self::new_with_source_expression(value, source_file, source_line, source_expression)
    }

    pub(crate) fn new_with_source_expression(
        value: Value<B>,
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

    pub fn value(&self) -> &Value<B> {
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
pub struct DeferredValueIf<B: BindingAddress = RelativeBindingName> {
    pub(crate) condition: DeferredIfCondition<B>,
    pub(crate) then_value: Value<B>,
    pub(crate) else_value: Value<B>,
}

impl<B: BindingAddress> DeferredValueIf<B> {
    pub(crate) fn new(
        condition: DeferredIfCondition<B>,
        then_value: Value<B>,
        else_value: Value<B>,
    ) -> Self {
        Self {
            condition,
            then_value,
            else_value,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<B> {
        &self.condition
    }

    pub fn then_value(&self) -> &Value<B> {
        &self.then_value
    }

    pub fn else_value(&self) -> &Value<B> {
        &self.else_value
    }

    pub fn values(&self) -> [&Value<B>; 3] {
        [self.condition.value(), &self.then_value, &self.else_value]
    }

    pub(crate) fn map<C: BindingAddress>(
        &self,
        mut map: impl FnMut(&Value<B>) -> Value<C>,
    ) -> DeferredValueIf<C> {
        self.try_map::<C, std::convert::Infallible>(|value| Ok(map(value)))
            .expect("an infallible deferred-if map cannot fail")
    }

    pub(crate) fn try_map<C: BindingAddress, E>(
        &self,
        mut map: impl FnMut(&Value<B>) -> Result<Value<C>, E>,
    ) -> Result<DeferredValueIf<C>, E> {
        Ok(DeferredValueIf::new(
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
}

#[derive(Debug, Clone)]
pub struct DeferredStructuralIf<B: BindingAddress = RelativeBindingName> {
    pub(crate) condition: DeferredIfCondition<B>,
    pub(crate) then_branch: DeferredIfBody<B>,
    pub(crate) else_branch: Option<DeferredIfBody<B>>,
}

impl<B: BindingAddress> DeferredStructuralIf<B> {
    pub(crate) fn new(
        condition: DeferredIfCondition<B>,
        then_branch: DeferredIfBody<B>,
        else_branch: Option<DeferredIfBody<B>>,
    ) -> Self {
        Self {
            condition,
            then_branch,
            else_branch,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<B> {
        &self.condition
    }

    pub fn then_branch(&self) -> &DeferredIfBody<B> {
        &self.then_branch
    }

    pub fn else_branch(&self) -> Option<&DeferredIfBody<B>> {
        self.else_branch.as_ref()
    }

    pub(crate) fn branches(&self) -> impl Iterator<Item = &DeferredIfBody<B>> {
        std::iter::once(&self.then_branch).chain(self.else_branch.iter())
    }

    pub(crate) fn branches_mut(&mut self) -> impl Iterator<Item = &mut DeferredIfBody<B>> {
        std::iter::once(&mut self.then_branch).chain(self.else_branch.iter_mut())
    }
}

#[derive(Debug, Clone)]
pub enum DeferredIfBody<B: BindingAddress = RelativeBindingName> {
    Resource(Box<Resource<ModuleSource, B>>),
    DataSource(Box<DataSource<ModuleSource, B>>),
    ModuleCall(ModuleCall<B>),
}

impl<B: BindingAddress> DeferredIfBody<B> {
    pub(crate) fn binding_address(&self) -> Option<&B> {
        match self {
            Self::Resource(resource) => resource.binding.as_ref(),
            Self::DataSource(data_source) => data_source.binding.as_ref(),
            Self::ModuleCall(call) => call.binding_name.as_ref(),
        }
    }

    pub(crate) fn binding(&self) -> Option<&str> {
        self.binding_address().map(BindingAddress::as_str)
    }
}

impl DeferredIfCondition<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredIfCondition<AbsoluteBindingAddress> {
        DeferredIfCondition::new_with_source_expression(
            self.value.resolve_bindings(resolve),
            self.source_file,
            self.source_line,
            self.source_expression,
        )
    }
}

impl DeferredStructuralIf<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredStructuralIf<AbsoluteBindingAddress> {
        DeferredStructuralIf::new(
            self.condition.resolve_bindings(resolve),
            self.then_branch.resolve_bindings(resolve),
            self.else_branch
                .map(|branch| branch.resolve_bindings(resolve)),
        )
    }
}

impl DeferredIfBody<RelativeBindingName> {
    fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredIfBody<AbsoluteBindingAddress> {
        match self {
            Self::Resource(resource) => {
                DeferredIfBody::Resource(Box::new(resource.resolve_bindings(resolve)))
            }
            Self::DataSource(data_source) => {
                DeferredIfBody::DataSource(Box::new(data_source.resolve_bindings(resolve)))
            }
            Self::ModuleCall(call) => DeferredIfBody::ModuleCall(call.resolve_bindings(resolve)),
        }
    }
}

impl DeferredStructuralIf<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        self.condition.value.map_absolute_bindings(map);
        self.then_branch.map_absolute_bindings(map);
        if let Some(branch) = &mut self.else_branch {
            branch.map_absolute_bindings(map);
        }
    }
}

impl DeferredIfBody<AbsoluteBindingAddress> {
    fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        match self {
            Self::Resource(resource) => resource.map_absolute_bindings(map),
            Self::DataSource(data_source) => data_source.map_absolute_bindings(map),
            Self::ModuleCall(call) => call.map_absolute_bindings(map),
        }
    }
}
