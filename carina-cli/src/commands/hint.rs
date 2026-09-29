use std::fmt;
use std::path::{Component, Path};

/// A copy-pasteable project-scoped `carina` command used in CLI hints.
///
/// The path is rendered as given by the caller rather than being resolved or
/// canonicalized here. Project-scoped callers pass the user-supplied project
/// path; saved-plan flows instead pass the plan's recorded (canonical) source
/// path for project-scoped hints. A hint that names the saved plan itself
/// passes the user-supplied plan path. Default paths made only of `.`
/// components are omitted; other paths appear after all subcommand arguments
/// as required by the CLI's positional argument layout.
pub(crate) struct ProjectCommand<'a> {
    subcommand: &'static str,
    argument: Option<CommandArgument<'a>>,
    project_dir: &'a Path,
}

enum CommandArgument<'a> {
    /// A positional value. Leading `-` values need an option terminator so
    /// clap does not interpret them as flags.
    Positional(&'a str),
    /// A path used as the value of an option already present in `subcommand`.
    /// Leading `-` paths are prefixed with `./` because inserting `--` between
    /// the option and its value would leave the option without an argument.
    Path(&'a Path),
}

impl<'a> ProjectCommand<'a> {
    pub(crate) fn new(subcommand: &'static str, project_dir: &'a Path) -> Self {
        Self {
            subcommand,
            argument: None,
            project_dir,
        }
    }

    /// Add a positional argument that must precede the project path, such as a
    /// lock ID. A leading `-` is protected with the `--` option terminator.
    pub(crate) fn with_argument(mut self, argument: &'a str) -> Self {
        self.argument = Some(CommandArgument::Positional(argument));
        self
    }

    /// Add a path argument that is the value of an option already included in
    /// the subcommand, such as the file following `plan --out`.
    pub(crate) fn with_path_argument(mut self, argument: &'a Path) -> Self {
        self.argument = Some(CommandArgument::Path(argument));
        self
    }
}

impl fmt::Display for ProjectCommand<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "carina {}", self.subcommand)?;
        if let Some(argument) = &self.argument {
            match argument {
                CommandArgument::Positional(argument) => {
                    if argument.starts_with('-') {
                        formatter.write_str(" --")?;
                    }
                    formatter.write_str(" ")?;
                    write_shell_word(formatter, argument)?;
                }
                CommandArgument::Path(argument) => {
                    formatter.write_str(" ")?;
                    write_path_word(formatter, argument)?;
                }
            }
        }

        if !is_default_project_dir(self.project_dir) {
            formatter.write_str(" ")?;
            write_path_word(formatter, self.project_dir)?;
        }

        Ok(())
    }
}

fn is_default_project_dir(project_dir: &Path) -> bool {
    project_dir
        .components()
        .all(|component| component == Component::CurDir)
}

fn write_path_word(formatter: &mut fmt::Formatter<'_>, path: &Path) -> fmt::Result {
    let is_relative = path.is_relative();
    let display = path.to_string_lossy();
    if is_relative && display.starts_with('-') {
        write_shell_word(formatter, &format!("./{display}"))
    } else {
        write_shell_word(formatter, &display)
    }
}

fn write_shell_word(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    if value.is_empty() {
        return formatter.write_str("''");
    }

    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_./+:@%-".contains(character))
    {
        return formatter.write_str(value);
    }

    formatter.write_str("'")?;
    for part in value.split_inclusive('\'') {
        if let Some(prefix) = part.strip_suffix('\'') {
            formatter.write_str(prefix)?;
            formatter.write_str("'\\''")?;
        } else {
            formatter.write_str(part)?;
        }
    }
    formatter.write_str("'")
}

#[cfg(test)]
mod tests {
    use super::ProjectCommand;
    use std::path::Path;

    #[test]
    fn omits_dot_project_dir() {
        for project_dir in [".", "./", "./."] {
            assert_eq!(
                ProjectCommand::new("plan", Path::new(project_dir)).to_string(),
                "carina plan",
                "project dir {project_dir:?} should be treated as the default",
            );
        }
    }

    #[test]
    fn omits_empty_project_dir() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("")).to_string(),
            "carina plan"
        );
    }

    #[test]
    fn appends_relative_project_dir() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("infra/foo")).to_string(),
            "carina plan infra/foo"
        );
    }

    #[test]
    fn prefixes_relative_project_dir_starting_with_dash() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("-foo")).to_string(),
            "carina plan ./-foo"
        );
    }

    #[test]
    fn appends_project_dir_after_subcommand_arguments() {
        assert_eq!(
            ProjectCommand::new("force-unlock", Path::new("infra/foo"))
                .with_argument("lock-123")
                .to_string(),
            "carina force-unlock lock-123 infra/foo"
        );
    }

    #[test]
    fn renders_empty_argument_as_an_empty_shell_word() {
        assert_eq!(
            ProjectCommand::new("force-unlock", Path::new("infra/foo"))
                .with_argument("")
                .to_string(),
            "carina force-unlock '' infra/foo"
        );
    }

    #[test]
    fn positional_argument_starting_with_dash_uses_option_terminator() {
        assert_eq!(
            ProjectCommand::new("force-unlock", Path::new("infra/foo"))
                .with_argument("-lock-id")
                .to_string(),
            "carina force-unlock -- -lock-id infra/foo"
        );
    }

    #[test]
    fn path_argument_starting_with_dash_is_prefixed() {
        assert_eq!(
            ProjectCommand::new("plan --out", Path::new("infra/foo"))
                .with_path_argument(Path::new("-plan.json"))
                .to_string(),
            "carina plan --out ./-plan.json infra/foo"
        );
    }

    #[test]
    fn appends_absolute_project_dir() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("/srv/projects/foo")).to_string(),
            "carina plan /srv/projects/foo"
        );
    }

    #[test]
    fn quotes_project_dir_containing_space() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("infra/foo bar")).to_string(),
            "carina plan 'infra/foo bar'"
        );
    }

    #[test]
    fn escapes_single_quote_in_project_dir() {
        assert_eq!(
            ProjectCommand::new("plan", Path::new("infra/team's")).to_string(),
            "carina plan 'infra/team'\\''s'"
        );
    }
}
