//! Core ownership of findings, independent of CLI or editor rendering.

use std::path::Path;

use crate::parser::{DeclarationLocation, ModuleCallId};

/// The source that owns a finding, including the identity of a concrete call.
/// Argument-dependent findings belong to that call, while declaration findings
/// retain their authored location and existing module-call presentation context.
/// carina#4461 case d selects ModuleArgument; #4492 can use the same owner for
/// argument-dependent empty/conflict findings without changing literal findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingOwner {
    Declaration {
        authored: Option<DeclarationLocation>,
        module_call: Option<ModuleCallId>,
    },
    ModuleArgument(ModuleCallId),
    /// A template has no concrete caller yet and cannot publish this obligation.
    AwaitingModuleArguments,
}

impl FindingOwner {
    pub fn location(&self) -> Option<&DeclarationLocation> {
        match self {
            Self::Declaration { authored, .. } => authored.as_ref(),
            Self::ModuleArgument(call) => Some(call.declaration_location()),
            Self::AwaitingModuleArguments => None,
        }
    }

    /// Publication is chosen in core. Renderers only select their current file.
    pub fn location_in(&self, file: &Path) -> Option<&DeclarationLocation> {
        self.location()
            .filter(|location| location.file().path() == file)
            .or_else(|| match self {
                Self::Declaration {
                    module_call: Some(call),
                    ..
                } => {
                    let location = call.declaration_location();
                    (location.file().path() == file).then_some(location)
                }
                _ => None,
            })
    }

    pub(crate) fn awaiting_arguments(&self) -> bool {
        matches!(self, Self::AwaitingModuleArguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_argument_owner_is_one_concrete_call_site() {
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "let child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n",
            Path::new("main.crn"),
            &context,
        )
        .unwrap();
        let first = FindingOwner::ModuleArgument(parsed.module_calls[0].id().clone());
        let second = FindingOwner::ModuleArgument(parsed.module_calls[1].id().clone());
        assert_ne!(first, second);
        assert_eq!(
            first.location(),
            Some(parsed.module_calls[0].id().declaration_location())
        );
        assert_eq!(first.location_in(Path::new("main.crn")), first.location());
        assert!(first.location_in(Path::new("child/main.crn")).is_none());
        assert!(FindingOwner::AwaitingModuleArguments.location().is_none());
    }

    #[test]
    fn declaration_owner_retains_existing_module_context() {
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let caller = crate::parser::parse_file(
            "let child = use { source = './child' }\nlet instance = child {}\n",
            Path::new("main.crn"),
            &context,
        )
        .unwrap();
        let child = crate::parser::parse_file(
            "mock.test.resource { name_prefix = '' }\n",
            Path::new("child/main.crn"),
            &context,
        )
        .unwrap();
        let authored = child.resources[0].declaration.location().unwrap();
        let owner = FindingOwner::Declaration {
            authored: Some(authored.clone()),
            module_call: Some(caller.module_calls[0].id().clone()),
        };
        assert_eq!(owner.location(), Some(authored));
        assert_eq!(
            owner.location_in(Path::new("child/main.crn")),
            Some(authored)
        );
        assert_eq!(
            owner.location_in(Path::new("main.crn")),
            Some(caller.module_calls[0].id().declaration_location())
        );
        assert!(owner.location_in(Path::new("other.crn")).is_none());
    }
}
