use std::fmt;

use crate::resource::ResolvedResourceId;

#[derive(Debug, Clone, Copy)]
enum Layout {
    Hint,
    LoopRewrite,
}

/// A state-block address rendered with the existing diagnostic escaping.
#[derive(Debug, Clone, Copy)]
pub struct MovedBlockAddress<'a> {
    provider: &'a str,
    resource_type: &'a str,
    identity: &'a str,
    layout: Layout,
}

impl<'a> MovedBlockAddress<'a> {
    pub fn new(id: &'a ResolvedResourceId) -> Self {
        Self {
            provider: id.provider(),
            resource_type: id.resource_type(),
            identity: id.identity_str(),
            layout: Layout::Hint,
        }
    }
}

impl fmt::Display for MovedBlockAddress<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.provider.is_empty() {
            write!(formatter, "{}.", self.provider)?;
        }
        let identity = match self.layout {
            Layout::Hint => render_identity_literal(self.identity),
            Layout::LoopRewrite => crate::parser::display_string_literal(self.identity),
        };
        write!(formatter, "{} {identity}", self.resource_type)
    }
}

/// The shared moved-block renderer for warnings and identity diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct MovedBlock<'a> {
    from: MovedBlockAddress<'a>,
    to: MovedBlockAddress<'a>,
    layout: Layout,
}

impl<'a> MovedBlock<'a> {
    pub fn new(from: &'a ResolvedResourceId, to: &'a ResolvedResourceId) -> Self {
        Self {
            from: MovedBlockAddress::new(from),
            to: MovedBlockAddress::new(to),
            layout: Layout::Hint,
        }
    }

    pub(super) fn for_loop_rewrite(
        provider: &'a str,
        resource_type: &'a str,
        from: &'a str,
        to: &'a str,
    ) -> Self {
        let address = |identity| MovedBlockAddress {
            provider,
            resource_type,
            identity,
            layout: Layout::LoopRewrite,
        };
        Self {
            from: address(from),
            to: address(to),
            layout: Layout::LoopRewrite,
        }
    }
}

impl fmt::Display for MovedBlock<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (indent, to_padding) = match self.layout {
            Layout::Hint => ("  ", "  "),
            Layout::LoopRewrite => ("", ""),
        };
        write!(
            formatter,
            "{indent}moved {{\n{indent}  from = {}\n{indent}  to{to_padding} = {}\n{indent}}}",
            self.from, self.to,
        )
    }
}

fn render_identity_literal(identity: &str) -> String {
    if !identity.contains('\'') && !identity.contains("${") {
        return format!("'{}'", identity.replace('\\', "\\\\"));
    }

    let mut escaped = String::with_capacity(identity.len());
    let mut chars = identity.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '$' if chars.peek() == Some(&'{') => escaped.push_str("\\$"),
            character => escaped.push(character),
        }
    }
    format!("\"{escaped}\"")
}
