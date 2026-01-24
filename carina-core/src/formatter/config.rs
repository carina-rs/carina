//! Formatting configuration

/// Formatting options
#[derive(Debug, Clone)]
pub struct FormatConfig {
    /// Number of spaces for indentation (default: 4)
    pub indent_size: usize,

    /// Use tabs instead of spaces for indentation
    pub use_tabs: bool,

    /// Number of blank lines between top-level blocks (default: 1)
    pub blank_lines_between_blocks: usize,

    /// Align attribute values in a block
    pub align_attributes: bool,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_size: 4,
            use_tabs: false,
            blank_lines_between_blocks: 1,
            align_attributes: true,
        }
    }
}

impl FormatConfig {
    /// Get the string to use for a single level of indentation
    pub fn indent_string(&self) -> String {
        if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.indent_size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = FormatConfig::default();
        assert_eq!(config.indent_size, 4);
        assert!(!config.use_tabs);
        assert_eq!(config.blank_lines_between_blocks, 1);
        assert!(config.align_attributes);
    }

    #[test]
    fn test_indent_string_spaces() {
        let config = FormatConfig::default();
        assert_eq!(config.indent_string(), "    ");
    }

    #[test]
    fn test_indent_string_tabs() {
        let config = FormatConfig {
            use_tabs: true,
            ..Default::default()
        };
        assert_eq!(config.indent_string(), "\t");
    }
}
