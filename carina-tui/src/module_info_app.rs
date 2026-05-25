//! Application state for the TUI module info viewer

use carina_core::module::{FileSignature, ModuleSignature, RootConfigSignature};
use ratatui::widgets::ListState;

use crate::app::FocusedPanel;

/// Section categories in the module info tree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Header,
    Argument,
    Resource,
    Attribute,
    Import,
    ModuleCall,
}

/// A row in the module info tree view
#[derive(Debug, Clone)]
pub struct InfoRow {
    pub label: String,
    pub type_info: String,
    /// Detail text for non-argument rows
    pub detail: String,
    pub depth: usize,
    pub kind: SectionKind,
    /// Whether this argument is required (only for Argument rows)
    pub required: bool,
    /// Default value (only for Argument rows)
    pub default_value: Option<String>,
    /// Description (only for Argument rows)
    pub description: Option<String>,
}

/// Application state for module info TUI
pub struct ModuleInfoApp {
    pub rows: Vec<InfoRow>,
    pub list_state: ListState,
    pub title: String,
    pub is_module: bool,
    pub focused_panel: FocusedPanel,
    pub detail_lines: Vec<String>,
}

impl ModuleInfoApp {
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
            list_state,
            title,
            is_module,
            focused_panel: FocusedPanel::Tree,
            detail_lines,
        }
    }

    pub fn selected(&self) -> usize {
        self.list_state.selected().unwrap_or(0)
    }

    fn build_module_rows(sig: &ModuleSignature) -> Vec<InfoRow> {
        let mut rows = Vec::new();

        rows.push(InfoRow::header(
            "ARGUMENTS",
            format!("{} items", sig.requires.len()),
        ));
        for arg in &sig.requires {
            rows.push(InfoRow {
                label: arg.name.clone(),
                type_info: format!("{}", arg.type_expr),
                detail: String::new(),
                depth: 1,
                kind: SectionKind::Argument,
                required: arg.required,
                default_value: arg.default.as_ref().map(|d| d.to_string()),
                description: arg.description.clone(),
            });
        }

        rows.push(InfoRow::header(
            "CREATES",
            format!("{} resources", sig.creates.len()),
        ));
        for creation in &sig.creates {
            rows.push(InfoRow::resource(
                &creation.binding_name,
                &format!("{}", creation.resource_type),
                &creation.dependencies,
            ));
        }

        rows.push(InfoRow::header(
            "ATTRIBUTES",
            format!("{} items", sig.exposes.len()),
        ));
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
            rows.push(InfoRow::simple(
                &attr.name,
                &type_str,
                &source,
                SectionKind::Attribute,
            ));
        }

        rows
    }

    fn build_root_config_rows(sig: &RootConfigSignature) -> Vec<InfoRow> {
        let mut rows = Vec::new();

        rows.push(InfoRow::header(
            "IMPORTS",
            format!("{} items", sig.imports.len()),
        ));
        for import in &sig.imports {
            rows.push(InfoRow::simple(
                &import.alias,
                "",
                &format!("path: {}", import.path),
                SectionKind::Import,
            ));
        }

        rows.push(InfoRow::header(
            "CREATES",
            format!(
                "{} resources, {} module calls",
                sig.resources.len(),
                sig.module_calls.len()
            ),
        ));
        for creation in &sig.resources {
            rows.push(InfoRow::resource(
                &creation.binding_name,
                &format!("{}", creation.resource_type),
                &creation.dependencies,
            ));
        }

        for call in &sig.module_calls {
            let binding = call
                .binding_name
                .as_ref()
                .map(|b| format!("{} = ", b))
                .unwrap_or_default();
            rows.push(InfoRow::simple(
                &format!("{}{}", binding, call.module_name),
                "",
                &if call.arguments.is_empty() {
                    String::new()
                } else {
                    format!("args: {}", call.arguments.join(", "))
                },
                SectionKind::ModuleCall,
            ));
        }

        rows
    }

    fn build_detail_for_row(row: &InfoRow) -> Vec<String> {
        let mut lines = vec![row.label.clone()];
        if !row.type_info.is_empty() {
            lines.push(format!("Type: {}", row.type_info));
        }
        if row.required {
            lines.push("required".to_string());
        }
        if let Some(default) = &row.default_value {
            lines.push(format!("default: {}", default));
        }
        if let Some(desc) = &row.description {
            lines.push(desc.clone());
        }
        if !row.detail.is_empty() {
            lines.push(row.detail.clone());
        }
        lines
    }

    pub fn move_up(&mut self) {
        let selected = self.selected();
        if selected > 0 {
            self.list_state.select(Some(selected - 1));
            self.update_detail();
        }
    }

    pub fn move_down(&mut self) {
        let selected = self.selected();
        if selected + 1 < self.rows.len() {
            self.list_state.select(Some(selected + 1));
            self.update_detail();
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Tree => FocusedPanel::Detail,
            FocusedPanel::Detail => FocusedPanel::Tree,
        };
    }

    fn update_detail(&mut self) {
        if let Some(row) = self.rows.get(self.selected()) {
            self.detail_lines = Self::build_detail_for_row(row);
        }
    }
}

use carina_core::module::TypedDependency;

impl InfoRow {
    fn header(label: &str, detail: String) -> Self {
        InfoRow {
            label: label.to_string(),
            type_info: String::new(),
            detail,
            depth: 0,
            kind: SectionKind::Header,
            required: false,
            default_value: None,
            description: None,
        }
    }

    fn simple(label: &str, type_info: &str, detail: &str, kind: SectionKind) -> Self {
        InfoRow {
            label: label.to_string(),
            type_info: type_info.to_string(),
            detail: detail.to_string(),
            depth: 1,
            kind,
            required: false,
            default_value: None,
            description: None,
        }
    }

    fn resource(binding_name: &str, resource_type: &str, deps: &[TypedDependency]) -> Self {
        let dep_detail = format_dep_detail(deps);
        InfoRow {
            label: binding_name.to_string(),
            type_info: resource_type.to_string(),
            detail: dep_detail,
            depth: 1,
            kind: SectionKind::Resource,
            required: false,
            default_value: None,
            description: None,
        }
    }
}

fn format_dep_detail(deps: &[TypedDependency]) -> String {
    if deps.is_empty() {
        return String::new();
    }
    let dep_names: Vec<String> = deps
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
}
