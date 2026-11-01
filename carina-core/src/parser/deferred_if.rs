//! Parser-template nodes for `if` expressions whose condition depends only on
//! module arguments.
//!
//! These nodes are resolved while a module call substitutes its arguments.
//! They must never survive into the expanded root graph, a saved plan, or
//! state.

use std::path::{Path, PathBuf};

use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, DataSource, ModuleSource, ParserPlaceholder,
    ValueKind, RelativeBindingName, Resource, Value,
};

use super::ModuleCall;

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredIfCondition<
    P: ValueKind = ParserPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    pub(crate) value: Value<P, B>,
    pub(crate) source_file: PathBuf,
    pub(crate) source_line: usize,
    pub(crate) source_expression: String,
}

impl DeferredIfCondition<ParserPlaceholder, RelativeBindingName> {
    #[cfg(test)]
    pub(crate) fn new(
        value: Value<ParserPlaceholder, RelativeBindingName>,
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

impl<P: ValueKind, B: BindingAddress> DeferredIfCondition<P, B> {
    pub(crate) fn new_with_source_expression(
        value: Value<P, B>,
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

    pub fn value(&self) -> &Value<P, B> {
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
pub struct DeferredValueIf<
    P: ValueKind = ParserPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    pub(crate) condition: DeferredIfCondition<P, B>,
    pub(crate) then_value: Value<P, B>,
    pub(crate) else_value: Value<P, B>,
}

impl<P: ValueKind, B: BindingAddress> DeferredValueIf<P, B> {
    pub(crate) fn new(
        condition: DeferredIfCondition<P, B>,
        then_value: Value<P, B>,
        else_value: Value<P, B>,
    ) -> Self {
        Self {
            condition,
            then_value,
            else_value,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<P, B> {
        &self.condition
    }

    pub fn then_value(&self) -> &Value<P, B> {
        &self.then_value
    }

    pub fn else_value(&self) -> &Value<P, B> {
        &self.else_value
    }

    pub fn values(&self) -> [&Value<P, B>; 3] {
        [self.condition.value(), &self.then_value, &self.else_value]
    }

    pub(crate) fn map(&self, mut map: impl FnMut(&Value<P, B>) -> Value<P, B>) -> Self {
        self.try_map::<std::convert::Infallible>(|value| Ok(map(value)))
            .expect("an infallible deferred-if map cannot fail")
    }

    pub(crate) fn try_map<E>(
        &self,
        mut map: impl FnMut(&Value<P, B>) -> Result<Value<P, B>, E>,
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

    pub(crate) fn try_map_stage<Q: ValueKind, E>(
        self,
        mut map: impl FnMut(Value<P, B>) -> Result<Value<Q, B>, E>,
    ) -> Result<DeferredValueIf<Q, B>, E> {
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

    pub(crate) fn map_bindings<T: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> T,
    ) -> DeferredValueIf<P, T> {
        DeferredValueIf::new(
            DeferredIfCondition::new_with_source_expression(
                self.condition.value.map_bindings(map),
                self.condition.source_file,
                self.condition.source_line,
                self.condition.source_expression,
            ),
            self.then_value.map_bindings(map),
            self.else_value.map_bindings(map),
        )
    }
}

#[derive(Debug, Clone)]
pub struct DeferredStructuralIf<
    P: ValueKind = ParserPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    pub(crate) condition: DeferredIfCondition<P, B>,
    pub(crate) then_branch: DeferredIfBody<P, B>,
    pub(crate) else_branch: Option<DeferredIfBody<P, B>>,
}

impl<P: ValueKind, B: BindingAddress> DeferredStructuralIf<P, B> {
    pub(crate) fn new(
        condition: DeferredIfCondition<P, B>,
        then_branch: DeferredIfBody<P, B>,
        else_branch: Option<DeferredIfBody<P, B>>,
    ) -> Self {
        Self {
            condition,
            then_branch,
            else_branch,
        }
    }

    pub fn condition(&self) -> &DeferredIfCondition<P, B> {
        &self.condition
    }

    pub fn then_branch(&self) -> &DeferredIfBody<P, B> {
        &self.then_branch
    }

    pub fn else_branch(&self) -> Option<&DeferredIfBody<P, B>> {
        self.else_branch.as_ref()
    }

    pub(crate) fn branches(&self) -> impl Iterator<Item = &DeferredIfBody<P, B>> {
        std::iter::once(&self.then_branch).chain(self.else_branch.iter())
    }

    pub(crate) fn branches_mut(&mut self) -> impl Iterator<Item = &mut DeferredIfBody<P, B>> {
        std::iter::once(&mut self.then_branch).chain(self.else_branch.iter_mut())
    }
}

#[derive(Debug, Clone)]
pub enum DeferredIfBody<
    P: ValueKind = ParserPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    Resource(Box<Resource<ModuleSource, P, B>>),
    DataSource(Box<DataSource<ModuleSource, P, B>>),
    ModuleCall(ModuleCall<P, B>),
}

impl<P: ValueKind, B: BindingAddress> DeferredIfBody<P, B> {
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

impl<P: ValueKind> DeferredIfCondition<P, RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredIfCondition<P, AbsoluteBindingAddress> {
        DeferredIfCondition::new_with_source_expression(
            self.value.resolve_bindings(resolve),
            self.source_file,
            self.source_line,
            self.source_expression,
        )
    }
}

impl<P: ValueKind> DeferredStructuralIf<P, RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredStructuralIf<P, AbsoluteBindingAddress> {
        DeferredStructuralIf::new(
            self.condition.resolve_bindings(resolve),
            self.then_branch.resolve_bindings(resolve),
            self.else_branch
                .map(|branch| branch.resolve_bindings(resolve)),
        )
    }
}

impl<P: ValueKind> DeferredIfBody<P, RelativeBindingName> {
    fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DeferredIfBody<P, AbsoluteBindingAddress> {
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

impl<P: ValueKind> DeferredStructuralIf<P, AbsoluteBindingAddress> {
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

impl<P: ValueKind> DeferredIfBody<P, AbsoluteBindingAddress> {
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
