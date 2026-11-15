//! Credential-free backend context shared by completion and hover.
use carina_core::validation::admission::configuration::backend::BackendKind;
use tower_lsp::lsp_types::Position;

pub(crate) enum Context {
    Header,
    Fields {
        kind: BackendKind,
        field: Option<&'static str>,
    },
}

pub(crate) fn context(text: &str, position: Position) -> Option<Context> {
    let lexed = crate::lex::lex(text);
    let cursor_line = lexed.lines().get(position.line as usize)?;
    let cursor_char = crate::position::line_prefix(cursor_line.text(), position.character)
        .chars()
        .count();
    if let Some(classified) = cursor_line.chars().get(cursor_char).or_else(|| {
        cursor_char
            .checked_sub(1)
            .and_then(|index| cursor_line.chars().get(index))
    }) && classified.kind() != crate::lex::LexKind::Code
    {
        return None;
    }
    let mut depth = 0_i32;
    let mut backend = None;
    for (index, line) in lexed.lines().iter().enumerate() {
        if index > position.line as usize {
            break;
        }
        let code = line.masked_code();
        let code = code.as_str();
        let prefix = if index == position.line as usize {
            crate::position::line_prefix(code, position.character)
        } else {
            code
        };
        if depth == 0 {
            if let Some(rest) = code
                .trim_start()
                .strip_prefix("backend")
                .filter(|rest| rest.starts_with(char::is_whitespace))
            {
                if index == position.line as usize && !prefix.contains('{') {
                    return Some(Context::Header);
                }
                let name = rest
                    .trim_start()
                    .split(|ch: char| ch.is_whitespace() || ch == '{')
                    .next()?;
                backend = BackendKind::from_name(name);
            } else {
                backend = None;
            }
        }
        for ch in prefix.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if index == position.line as usize && depth == 1 {
            return backend.map(|kind| Context::Fields {
                kind,
                field: field_key(kind, code, position),
            });
        }
    }
    None
}

fn field_key(kind: BackendKind, code: &str, position: Position) -> Option<&'static str> {
    let prefix = crate::position::line_prefix(code, position.character);
    let identifier = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
    let start = prefix
        .rmatch_indices(|ch: char| !identifier(ch))
        .next()
        .map_or(0, |(index, separator)| index + separator.len());
    let end = prefix.len()
        + code[prefix.len()..]
            .find(|ch: char| !identifier(ch))
            .unwrap_or(code.len() - prefix.len());
    if !code[end..].trim_start().starts_with('=') {
        return None;
    }
    kind.fields()
        .find(|(name, _, _)| *name == &code[start..end])
        .map(|(name, _, _)| name)
}

pub(crate) fn snippet(kind: BackendKind) -> String {
    let fields = kind
        .fields()
        .enumerate()
        .map(|(index, (name, ty, _))| {
            if matches!(ty.shape_ref_free(), Ok(carina_core::schema::Shape::Bool)) {
                format!("    {name} = ${{{}:true}}", index + 1)
            } else {
                format!("    {name} = '${{{}:{name}}}'", index + 1)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{} {{\n{fields}\n}}", kind.name())
}
