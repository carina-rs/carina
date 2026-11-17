//! Completions at the member level of `arguments` blocks.

use super::CompletionProvider;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

impl CompletionProvider {
    pub(super) fn arguments_member_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "require".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("require ${1:condition}, '${2:error message}'".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("Cross-argument constraint".to_string()),
            ..Default::default()
        }]
    }
}
