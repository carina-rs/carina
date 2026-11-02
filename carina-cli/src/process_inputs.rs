//! Executable-only adapter from process inputs to owned library inputs.
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_cli::cursor::CursorState;
use carina_cli::error::{AppError, CommandInputError};
use carina_cli::output_style::ColorMode;
use carina_cli::plan_image::{
    ImageTheme, PlanImageError, PlanImageTarget, ResolvedPlanImageOptions,
    resolve_plan_image_options,
};
use carina_cli::runtime::{BuiltinMockInputs, CommandInputs, CommandRuntime, InvocationHooks};
use carina_cli::wiring::TrustedLocalGrantInputs;
use carina_core::EnvironmentSnapshot;
use carina_provider_mock::{
    ConfiguredBuiltinMock, DecodedMockControls, MockScope, decode_controls,
};

pub(super) struct ProcessInputs {
    environment: EnvironmentSnapshot,
    cwd: Arc<dyn Fn() -> std::io::Result<PathBuf> + Send + Sync>,
    cli_grants: Vec<String>,
    stdout_is_terminal: bool,
    pub color_mode: ColorMode,
    pub cursor: CursorState,
}

impl ProcessInputs {
    pub fn environment(&self) -> &EnvironmentSnapshot {
        &self.environment
    }

    pub fn capture(cli_grants: Vec<String>) -> Self {
        #[expect(clippy::disallowed_methods)]
        let environment = EnvironmentSnapshot::capture_process();
        Self::from_environment(
            environment,
            std::env::current_dir,
            cli_grants,
            std::io::stdout().is_terminal(),
        )
    }

    pub fn plan_image_options(
        &self,
        target: Option<&PlanImageTarget>,
        inline: bool,
        theme: Option<ImageTheme>,
    ) -> Result<ResolvedPlanImageOptions, PlanImageError> {
        resolve_plan_image_options(
            target,
            inline,
            theme,
            |name| {
                self.environment
                    .get(name)
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
            },
            self.stdout_is_terminal,
        )
    }

    fn from_environment(
        environment: EnvironmentSnapshot,
        cwd: impl Fn() -> std::io::Result<PathBuf> + Send + Sync + 'static,
        cli_grants: Vec<String>,
        terminal: bool,
    ) -> Self {
        let color_mode = ColorMode::resolve(
            terminal,
            environment.get("NO_COLOR"),
            environment.get("CLICOLOR"),
            environment.get("CLICOLOR_FORCE"),
        );
        Self {
            environment,
            cwd: Arc::new(cwd),
            cli_grants,
            stdout_is_terminal: terminal,
            color_mode,
            cursor: CursorState::new(),
        }
    }

    pub fn runtime(
        &self,
        user_dirs: carina_core::UserDirs,
    ) -> Result<CommandRuntime, Box<AppError>> {
        self.runtime_with_consent(user_dirs, None)
    }

    pub fn init_runtime(
        &self,
        user_dirs: carina_core::UserDirs,
        project: &Path,
        accepted: &[String],
    ) -> Result<CommandRuntime, Box<AppError>> {
        self.runtime_with_consent(user_dirs, Some((project, accepted)))
    }

    fn runtime_with_consent(
        &self,
        user_dirs: carina_core::UserDirs,
        consent: Option<(&Path, &[String])>,
    ) -> Result<CommandRuntime, Box<AppError>> {
        let DecodedMockControls {
            state_file,
            behavior,
        } = decode_controls(&self.environment);
        let cwd = self.cwd.clone();
        let builtin_mock = BuiltinMockInputs::deferred(move || {
            let state_file = state_file.unwrap_or_else(|| PathBuf::from(".carina/state.json"));
            let state_file = if state_file.is_absolute() {
                state_file
            } else {
                cwd()
                    .map_err(|error| CommandInputError::StartupDirectory { error })?
                    .join(state_file)
            };
            let scope = MockScope::file(state_file.clone()).map_err(|error| {
                CommandInputError::MockStorage {
                    path: carina_core::paths::DisplayPath::new(&state_file)
                        .expect("mock storage input is absolute"),
                    error,
                }
            })?;
            Ok(ConfiguredBuiltinMock::new(behavior, scope))
        });
        let accepted_egress = if let Some((project, accepted)) = consent {
            let project = carina_core::paths::ResolvedPath::new(project)
                .map_err(|error| carina_cli::commands::init::InitError::Other(error.to_string()))
                .map_err(AppError::from)?;
            carina_provider_resolver::AcceptedEgressSources::parse_os(
                accepted,
                self.environment.get("CARINA_ACCEPT_EGRESS"),
                &project,
            )
            .map_err(carina_cli::commands::init::InitError::AcceptedEgress)
            .map_err(AppError::from)?
        } else {
            Default::default()
        };
        Ok(CommandRuntime::new(
            user_dirs,
            CommandInputs {
                environment: self.environment.clone(),
                builtin_mock,
                trusted_local_grants: TrustedLocalGrantInputs::new(
                    self.cli_grants.clone(),
                    self.environment
                        .get("CARINA_TRUST_LOCAL_PROVIDER")
                        .map(std::ffi::OsStr::to_os_string),
                ),
                accepted_egress,
                color_mode: self.color_mode,
                cursor: self.cursor.clone(),
                hooks: InvocationHooks {
                    plan_drift_handshake_dir: self
                        .environment
                        .get("CARINA_TEST_PLAN_DRIFT_HANDSHAKE_DIR")
                        .map(PathBuf::from),
                    capture_live_apply_plan: self
                        .environment
                        .get("CARINA_TEST_CAPTURE_LIVE_APPLY_PLAN")
                        .map(PathBuf::from),
                },
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    impl ProcessInputs {
        fn from_values(
            values: BTreeMap<String, OsString>,
            cwd: impl Fn() -> std::io::Result<PathBuf> + Send + Sync + 'static,
            cli_grants: Vec<String>,
            terminal: bool,
        ) -> Self {
            Self::from_environment(EnvironmentSnapshot::new(values), cwd, cli_grants, terminal)
        }
    }

    #[test]
    fn environment_snapshot_reaches_the_command_runtime() {
        let inputs = ProcessInputs::from_values(
            [("CARINA_ENV_SNAPSHOT".to_owned(), OsString::from("captured"))].into(),
            || panic!("environment evaluation does not need startup cwd"),
            Vec::new(),
            false,
        );
        let dirs = carina_core::TestUserDirs::new();
        let runtime = inputs.runtime((*dirs).clone()).unwrap();
        let context = carina_core::parser::ProviderContext::new(runtime.environment().clone());
        let mut parsed = carina_core::parser::parse_file(
            "let value = env('CARINA_ENV_SNAPSHOT')",
            Path::new("test.crn"),
            &context,
        )
        .unwrap();
        carina_core::parser::resolve_resource_refs_with_config(&mut parsed, &context).unwrap();
        assert_eq!(
            carina_core::value::format_value(&parsed.variables["value"]),
            "'captured'",
        );
    }

    #[test]
    fn plan_image_options_use_supplied_environment_and_terminal_flag() {
        use carina_cli::plan_image::InlineImageProtocol;

        let supplied = |terminal, program: &str| {
            ProcessInputs::from_values(
                [("TERM_PROGRAM".to_owned(), OsString::from(program))].into(),
                || panic!("plan image options do not need startup cwd"),
                Vec::new(),
                terminal,
            )
        };
        let kitty = supplied(true, "ghostty")
            .plan_image_options(None, true, Some(ImageTheme::Light))
            .unwrap();
        assert_eq!(kitty.inline().unwrap().protocol, InlineImageProtocol::Kitty);
        let iterm = supplied(true, "iTerm.app")
            .plan_image_options(None, true, Some(ImageTheme::Dark))
            .unwrap();
        assert_eq!(
            iterm.inline().unwrap().protocol,
            InlineImageProtocol::Iterm2
        );
        assert!(matches!(
            supplied(false, "ghostty").plan_image_options(None, true, Some(ImageTheme::Light)),
            Err(PlanImageError::InlineImageStdoutNotTerminal)
        ));
        assert!(matches!(
            supplied(true, "tmux").plan_image_options(None, true, Some(ImageTheme::Light)),
            Err(PlanImageError::InlineImageTmuxUnsupported)
        ));
    }

    #[test]
    fn missing_startup_directory_only_fails_for_a_consumer_that_needs_it() {
        let inputs = ProcessInputs::from_values(
            BTreeMap::new(),
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "startup directory removed",
                ))
            },
            Vec::new(),
            false,
        );
        let dirs = carina_core::TestUserDirs::new();
        let runtime = inputs
            .runtime((*dirs).clone())
            .expect("runtime construction must leave cwd unresolved");
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("main.crn");
        std::fs::write(&source, "provider other {}\n").unwrap();
        carina_cli::commands::fmt::run_fmt(
            &runtime,
            project.path(),
            carina_cli::commands::fmt::FmtMode::Preview,
            false,
        )
        .expect("non-mock formatting at an absolute project path must not need startup cwd");

        std::fs::write(&source, "provider mock {}\n").unwrap();
        let error = carina_cli::commands::validate::run_validate(
            &runtime,
            project.path(),
            false,
            &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        )
        .expect_err("relative mock storage must report the missing startup cwd");
        assert!(matches!(error, AppError::CommandInput(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "Failed to resolve the command startup directory: startup directory removed"
        );
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn absolute_mock_storage_does_not_resolve_startup_directory() {
        let project = tempfile::tempdir().unwrap();
        let inputs = ProcessInputs::from_values(
            [(
                "CARINA_MOCK_STATE_FILE".to_owned(),
                project.path().join("mock-state.json").into_os_string(),
            )]
            .into(),
            || panic!("absolute mock storage does not need startup cwd"),
            Vec::new(),
            false,
        );
        let dirs = carina_core::TestUserDirs::new();
        let runtime = inputs.runtime((*dirs).clone()).unwrap();
        std::fs::write(project.path().join("main.crn"), "provider mock {}\n").unwrap();
        carina_cli::commands::validate::run_validate(
            &runtime,
            project.path(),
            false,
            &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        )
        .expect("absolute mock storage must work without startup cwd");
    }

    #[test]
    fn invalid_mock_storage_only_fails_when_builtin_mock_is_selected() {
        let project = tempfile::tempdir().unwrap();
        let inputs = ProcessInputs::from_values(
            [("CARINA_MOCK_STATE_FILE".to_owned(), OsString::from("/"))].into(),
            || Ok(PathBuf::from("/supplied/start")),
            Vec::new(),
            false,
        );
        let dirs = carina_core::TestUserDirs::new();
        let runtime = inputs
            .runtime((*dirs).clone())
            .expect("unused mock inputs must not prevent runtime construction");
        let source = project.path().join("main.crn");
        std::fs::write(&source, "provider other {}\n").unwrap();
        carina_cli::commands::fmt::run_fmt(
            &runtime,
            project.path(),
            carina_cli::commands::fmt::FmtMode::Preview,
            false,
        )
        .expect("a non-mock formatting command must not resolve mock storage");

        for configuration in ["provider mock {}\n", "// implicit mock\n"] {
            std::fs::write(&source, configuration).unwrap();
            let error = carina_cli::commands::validate::run_validate(
                &runtime,
                project.path(),
                false,
                &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
            )
            .expect_err("selecting either builtin mock route must reject invalid storage");
            assert!(matches!(error, AppError::CommandInput(_)), "{error:?}");
            assert_eq!(
                error.to_string(),
                "Invalid mock state storage path /: mock state path has no file name"
            );
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn binary_color_policy_uses_only_supplied_controls() {
        let captured = |terminal, values: &[(&str, &str)]| {
            ProcessInputs::from_values(
                values
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
                    .collect(),
                || Ok(PathBuf::from("/supplied/start")),
                Vec::new(),
                terminal,
            )
            .color_mode
        };
        assert_eq!(captured(false, &[]), ColorMode::Plain);
        assert_eq!(captured(true, &[]), ColorMode::Ansi);
        assert_eq!(captured(true, &[("NO_COLOR", "")]), ColorMode::Plain);
        assert_eq!(captured(true, &[("CLICOLOR", "0")]), ColorMode::Plain);
        assert_eq!(
            captured(false, &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]),
            ColorMode::Ansi
        );
        assert_eq!(
            captured(false, &[("CLICOLOR_FORCE", "0")]),
            ColorMode::Plain
        );
        use std::os::unix::ffi::OsStringExt;
        let invalid = OsString::from_vec(vec![0xff]);
        let values = [
            ("NO_COLOR".to_owned(), invalid.clone()),
            ("CLICOLOR_FORCE".to_owned(), invalid),
        ]
        .into();
        let inputs = ProcessInputs::from_values(
            values,
            || Ok(PathBuf::from("/supplied/start")),
            Vec::new(),
            true,
        );
        assert_eq!(inputs.color_mode, ColorMode::Ansi);
    }
}
