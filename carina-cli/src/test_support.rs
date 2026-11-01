//! Each fixture owns its user directories, builtin store and command runtime.
use crate::cursor::CursorState;
use crate::output_style::ColorMode;
use crate::runtime::{BuiltinMockInputs, CommandInputs, CommandRuntime, InvocationHooks};
use crate::wiring::TrustedLocalGrantInputs;
use carina_core::TestUserDirs;
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use std::sync::{Arc, OnceLock};

/// Builds one immutable invocation from the fixture's mock configuration.
/// Reconfiguration starts a new invocation; existing runtime clones stay valid.
/// Installed hooks belong to the fixture and survive invocation rebuilds.
pub struct TestCommandRuntime {
    dirs: Arc<TestUserDirs>,
    behavior: MockBehavior,
    scope: MockScope,
    color_mode: ColorMode,
    hooks: InvocationHooks,
    runtime: OnceLock<CommandRuntime>,
}
impl TestCommandRuntime {
    pub fn new() -> Self {
        Self::with_mock(MockBehavior::default(), MockScope::memory())
    }
    pub fn set_mock(&mut self, behavior: MockBehavior, scope: MockScope) {
        self.update_mock(|current_behavior, current_scope| {
            *current_behavior = behavior;
            *current_scope = scope;
        });
    }
    pub(crate) fn update_mock(&mut self, update: impl FnOnce(&mut MockBehavior, &mut MockScope)) {
        update(&mut self.behavior, &mut self.scope);
        self.runtime.take();
    }
    pub fn with_mock(behavior: MockBehavior, scope: MockScope) -> Self {
        Self {
            dirs: Arc::new(TestUserDirs::new()),
            behavior,
            scope,
            color_mode: ColorMode::Plain,
            hooks: InvocationHooks::default(),
            runtime: OnceLock::new(),
        }
    }
    pub fn with_color_mode(mut self, color_mode: ColorMode) -> Self {
        self.color_mode = color_mode;
        self.runtime.take();
        self
    }
    fn build(&self) -> CommandRuntime {
        let configured = ConfiguredBuiltinMock::new(self.behavior.clone(), self.scope.clone());
        let mut runtime = CommandRuntime::new(
            (**self.dirs).clone(),
            CommandInputs {
                builtin_mock: BuiltinMockInputs::deferred(move || Ok(configured)),
                trusted_local_grants: TrustedLocalGrantInputs::new(Vec::new(), None),
                accepted_egress: Default::default(),
                color_mode: self.color_mode,
                cursor: CursorState::new(),
                hooks: self.hooks.clone(),
            },
        );
        runtime.directory_owner = Some(self.dirs.clone());
        runtime
    }
}
impl Default for TestCommandRuntime {
    fn default() -> Self {
        Self::new()
    }
}
impl std::ops::Deref for TestCommandRuntime {
    type Target = CommandRuntime;
    fn deref(&self) -> &Self::Target {
        self.runtime.get_or_init(|| self.build())
    }
}

#[cfg(test)]
pub(crate) use graph_fixtures::{compiled_loop, compiled_replace, persisted, resolved};

#[cfg(test)]
mod graph_fixtures {
    use carina_core::{
        effect::{DeferredCreatePayload, DeferredReplacePayload, NonEmptyDeletes},
        parser::{ExpandedDeferredForExpression, ExpandedParsedFile},
        resource::{PersistedDestroyDependencies, ResolvedResourceId, Resource},
    };
    pub fn compiled_loop(template: Box<ExpandedDeferredForExpression>) -> DeferredCreatePayload {
        let mut configuration = ExpandedParsedFile::default();
        configuration.upstream_states = ["orgs", "upstream"]
            .into_iter()
            .map(|binding| carina_core::parser::UpstreamState {
                binding: crate::absolute_binding_for_test(binding),
                source: "upstream".into(),
            })
            .collect();
        configuration.resources = ["cert", "parent", "primary", "fallback"]
            .into_iter()
            .map(|binding| {
                Resource::with_provider("mock", "test.resource", binding, None)
                    .with_absolute_binding(crate::absolute_binding_for_test(binding))
            })
            .collect();
        let graph = carina_core::resolved_graph::resolve_expanded_graph(configuration.into())
            .expect("fixture graph");
        let (template, dependency_template) = graph
            .compile_deferred_for(*template)
            .expect("fixture loop dependencies");
        DeferredCreatePayload::new(Box::new(template), dependency_template)
    }
    pub fn compiled_replace(
        deletes: NonEmptyDeletes,
        template: Box<ExpandedDeferredForExpression>,
    ) -> DeferredReplacePayload {
        let payload = compiled_loop(template);
        DeferredReplacePayload {
            deletes,
            template: payload.template,
            dependency_template: payload.dependency_template,
        }
    }
    pub fn persisted(
        rows: impl IntoIterator<Item = ResolvedResourceId>,
    ) -> PersistedDestroyDependencies {
        PersistedDestroyDependencies::from_test_dependencies(rows)
    }
    pub fn resolved(resource: Resource) -> carina_core::resource::ResolvedResource {
        carina_core::resource::ResolvedResource::collect_resolved([resource])
            .expect("fixture graph resolves")
            .remove(0)
    }
}
