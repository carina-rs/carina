//! Application state for the TUI module info viewer

use carina_core::module::{FileSignature, ModuleSignature, RootConfigSignature};
use ratatui::widgets::ListState;

/// Section categories in the module info tree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// Section header (ARGUMENTS, CREATES, ATTRIBUTES, IMPORTS)
    Header,
    /// Argument entry
    Argument,
    /// Resource creation entry
    Resource,
    /// Attribute/output entry
    Attribute,
    /// Import entry
    Import,
    /// Module call entry
    ModuleCall,
}

/// A row in the module info tree view
#[derive(Debug, Clone)]
pub struct InfoRow {
    /// Display text (main label)
    pub label: String,
    /// Optional type annotation
    pub type_info: String,
    /// Optional detail text (default value, description, etc.)
    pub detail: String,
    /// Nesting depth (0 = section header, 1 = entry)
    pub depth: usize,
    /// Kind of this row (for styling)
    pub kind: SectionKind,
}

/// Which panel has focus
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPanel {
    Tree,
    Detail,
}

/// Application state for module info TUI
pub struct ModuleInfoApp {
    /// All rows in the tree view
    pub rows: Vec<InfoRow>,
    /// Currently selected row index
    pub selected: usize,
    /// List state for ratatui scrolling
    pub list_state: ListState,
    /// Module/file name for the title
    pub title: String,
    /// Whether this is a module or root config
    pub is_module: bool,
    /// Which panel has focus
    pub focused_panel: FocusedPanel,
    /// Detail text for the currently selected row
    pub detail_lines: Vec<String>,
}

impl ModuleInfoApp {
    /// Create a new module info app from a FileSignature
    pub fn new(signature: &FileSignature) -> Self {
        let (rows, title, is_module) = match signature {
            FileSignature::Module(sig) => (Self::build_module_rows(sig), sig.name.clone(), true),
            FileSignature::RootConfig(sig) => {
                (Self::build_root_config_rows(sig), sig.name.clone(), false)
            }
        };

        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }

        let detail_lines = if !rows.is_empty() {
            Self::build_detail_for_row(&rows[0])
        } else {
            Vec::new()
        };

        ModuleInfoApp {
            rows,
            selected: 0,
            list_state,
            title,
            is_module,
            focused_panel: FocusedPanel::Tree,
            detail_lines,
        }
    }

    fn build_module_rows(sig: &ModuleSignature) -> Vec<InfoRow> {
        let mut rows = Vec::new();

        // ARGUMENTS section
        rows.push(InfoRow {
            label: "ARGUMENTS".to_string(),
            type_info: String::new(),
            detail: format!("{} items", sig.requires.len()),
            depth: 0,
            kind: SectionKind::Header,
        });
        for arg in &sig.requires {
            let type_str = format!("{}", arg.type_expr);
            let mut detail_parts = Vec::new();
            if arg.required {
                detail_parts.push("required".to_string());
            }
            if let Some(default) = &arg.default {
                detail_parts.push(format!("default: {}", default));
            }
            if let Some(desc) = &arg.description {
                detail_parts.push(desc.clone());
            }
            rows.push(InfoRow {
                label: arg.name.clone(),
                type_info: type_str,
                detail: detail_parts.join(" | "),
                depth: 1,
                kind: SectionKind::Argument,
            });
        }

        // CREATES section
        rows.push(InfoRow {
            label: "CREATES".to_string(),
            type_info: String::new(),
            detail: format!("{} resources", sig.creates.len()),
            depth: 0,
            kind: SectionKind::Header,
        });
        for creation in &sig.creates {
            let dep_detail = if creation.dependencies.is_empty() {
                String::new()
            } else {
                let dep_names: Vec<String> = creation
                    .dependencies
                    .iter()
                    .map(|d| {
                        if d.attribute.is_empty() {
                            format!("{} (via {})", d.target, d.used_in)
                        } else {
                            format!("{}.{} (via {})", d.target, d.attribute, d.used_in)
                        }
                    })
                    .collect();
                format!("depends on: {}", dep_names.join(", "))
            };
            rows.push(InfoRow {
                label: creation.binding_name.clone(),
                type_info: format!("{}", creation.resource_type),
                detail: dep_detail,
                depth: 1,
                kind: SectionKind::Resource,
            });
        }

        // ATTRIBUTES section
        rows.push(InfoRow {
            label: "ATTRIBUTES".to_string(),
            type_info: String::new(),
            detail: format!("{} items", sig.exposes.len()),
            depth: 0,
            kind: SectionKind::Header,
        });
        for attr in &sig.exposes {
            let type_str = attr
                .type_expr
                .as_ref()
                .map(|t| format!("{}", t))
                .unwrap_or_default();
            let source = attr
                .source_binding
                .as_ref()
                .map(|s| format!("from: {}", s))
                .unwrap_or_default();
            rows.push(InfoRow {
                label: attr.name.clone(),
                type_info: type_str,
                detail: source,
                depth: 1,
                kind: SectionKind::Attribute,
            });
        }

        rows
    }

    fn build_root_config_rows(sig: &RootConfigSignature) -> Vec<InfoRow> {
        let mut rows = Vec::new();

        // IMPORTS section
        rows.push(InfoRow {
            label: "IMPORTS".to_string(),
            type_info: String::new(),
            detail: format!("{} items", sig.imports.len()),
            depth: 0,
            kind: SectionKind::Header,
        });
        for import in &sig.imports {
            rows.push(InfoRow {
                label: import.alias.clone(),
                type_info: String::new(),
                detail: format!("path: {}", import.path),
                depth: 1,
                kind: SectionKind::Import,
            });
        }

        // CREATES section
        rows.push(InfoRow {
            label: "CREATES".to_string(),
            type_info: String::new(),
            detail: format!(
                "{} resources, {} module calls",
                sig.resources.len(),
                sig.module_calls.len()
            ),
            depth: 0,
            kind: SectionKind::Header,
        });
        for creation in &sig.resources {
            let dep_detail = if creation.dependencies.is_empty() {
                String::new()
            } else {
                let dep_names: Vec<String> = creation
                    .dependencies
                    .iter()
                    .map(|d| {
                        if d.attribute.is_empty() {
                            format!("{} (via {})", d.target, d.used_in)
                        } else {
                            format!("{}.{} (via {})", d.target, d.attribute, d.used_in)
                        }
                    })
                    .collect();
                format!("depends on: {}", dep_names.join(", "))
            };
            rows.push(InfoRow {
                label: creation.binding_name.clone(),
                type_info: format!("{}", creation.resource_type),
                detail: dep_detail,
                depth: 1,
                kind: SectionKind::Resource,
            });
        }

        // Module calls
        for call in &sig.module_calls {
            let binding = call
                .binding_name
                .as_ref()
                .map(|b| format!("{} = ", b))
                .unwrap_or_default();
            rows.push(InfoRow {
                label: format!("{}{}", binding, call.module_name),
                type_info: String::new(),
                detail: if call.arguments.is_empty() {
                    String::new()
                } else {
                    format!("args: {}", call.arguments.join(", "))
                },
                depth: 1,
                kind: SectionKind::ModuleCall,
            });
        }

        rows
    }

    /// Build detail lines for a selected row
    fn build_detail_for_row(row: &InfoRow) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(row.label.to_string());
        if !row.type_info.is_empty() {
            lines.push(format!("Type: {}", row.type_info));
        }
        if !row.detail.is_empty() {
            // Split detail by " | " for multi-line display
            for part in row.detail.split(" | ") {
                lines.push(part.to_string());
            }
        }
        lines
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.list_state.select(Some(self.selected));
            self.update_detail();
        }
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
            self.list_state.select(Some(self.selected));
            self.update_detail();
        }
    }

    /// Toggle focus between tree and detail panels
    pub fn toggle_focus(&mut self) {
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Tree => FocusedPanel::Detail,
            FocusedPanel::Detail => FocusedPanel::Tree,
        };
    }

    fn update_detail(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            self.detail_lines = Self::build_detail_for_row(row);
        }
    }
}
