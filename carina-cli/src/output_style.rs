//! Rendering policy is supplied by each invocation, never a process override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    Ansi,
    Plain,
}

impl ColorMode {
    /// Resolve captured terminal/environment inputs without consulting globals.
    pub fn resolve(
        terminal: bool,
        no_color: Option<&std::ffi::OsStr>,
        clicolor: Option<&std::ffi::OsStr>,
        force: Option<&std::ffi::OsStr>,
    ) -> Self {
        fn text(value: Option<&std::ffi::OsStr>) -> Option<&str> {
            value.and_then(std::ffi::OsStr::to_str)
        }
        let enabled = if text(force).is_some_and(|value| value != "0") {
            true
        } else if text(no_color).is_some() || text(clicolor).is_some_and(|value| value == "0") {
            false
        } else {
            terminal
        };
        if enabled { Self::Ansi } else { Self::Plain }
    }
}

#[derive(Clone, Debug)]
pub struct ColoredString {
    input: String,
    mode: ColorMode,
    foreground: Option<u8>,
    bold: bool,
    dimmed: bool,
    strikethrough: bool,
}

pub trait Colorize: Sized {
    fn styled(self, mode: ColorMode) -> ColoredString;
    fn red(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(31)
    }
    fn green(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(32)
    }
    fn yellow(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(33)
    }
    fn blue(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(34)
    }
    fn magenta(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(35)
    }
    fn cyan(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(36)
    }
    fn white(self, mode: ColorMode) -> ColoredString {
        self.styled(mode).foreground(37)
    }
    fn bold(self, mode: ColorMode) -> ColoredString {
        let mut value = self.styled(mode);
        value.bold = true;
        value
    }
    fn dimmed(self, mode: ColorMode) -> ColoredString {
        let mut value = self.styled(mode);
        value.dimmed = true;
        value
    }
    fn strikethrough(self, mode: ColorMode) -> ColoredString {
        let mut value = self.styled(mode);
        value.strikethrough = true;
        value
    }
    fn normal(self, mode: ColorMode) -> ColoredString {
        ColoredString {
            input: self.styled(mode).input,
            mode,
            foreground: None,
            bold: false,
            dimmed: false,
            strikethrough: false,
        }
    }
}
impl Colorize for &str {
    fn styled(self, mode: ColorMode) -> ColoredString {
        ColoredString {
            input: self.to_owned(),
            mode,
            foreground: None,
            bold: false,
            dimmed: false,
            strikethrough: false,
        }
    }
}
impl Colorize for ColoredString {
    fn styled(mut self, mode: ColorMode) -> ColoredString {
        self.mode = mode;
        self
    }
}
impl ColoredString {
    fn foreground(mut self, code: u8) -> Self {
        self.foreground = Some(code);
        self
    }
}
impl std::fmt::Display for ColoredString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.mode {
            ColorMode::Plain => return std::fmt::Display::fmt(&self.input, f),
            ColorMode::Ansi => {}
        }
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1".to_owned());
        }
        if self.dimmed {
            codes.push("2".to_owned());
        }
        if self.strikethrough {
            codes.push("9".to_owned());
        }
        if let Some(code) = self.foreground {
            codes.push(code.to_string());
        }
        if codes.is_empty() {
            return std::fmt::Display::fmt(&self.input, f);
        }
        let style = format!("\x1b[{}m", codes.join(";"));
        f.write_str(&style)?;
        std::fmt::Display::fmt(
            &self.input.replace("\x1b[0m", &format!("\x1b[0m{style}")),
            f,
        )?;
        f.write_str("\x1b[0m")
    }
}
