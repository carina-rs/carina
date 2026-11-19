//! Core ownership of findings, independent of CLI or editor rendering.

use std::path::Path;

use crate::parser::{DeclarationLocation, ModuleCallId};

/// An authored declaration and its existing module presentation context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingOwner {
    Declaration {
        authored: Option<DeclarationLocation>,
        module_call: Option<ModuleCallId>,
    },
}

impl FindingOwner {
    pub fn location(&self) -> Option<&DeclarationLocation> {
        match self {
            Self::Declaration { authored, .. } => authored.as_ref(),
        }
    }

    /// Existing module context for findings that project onto an importing file.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
