/// A single embedded document.
pub struct EmbeddedDoc {
    /// Short identifier shown in `--list` (e.g. "getting-started/quick-start")
    pub name: &'static str,
    /// Human-readable title
    pub title: &'static str,
    /// The full markdown content
    pub content: &'static str,
}

/// All documents embedded at compile time.
pub fn embedded_docs() -> Vec<EmbeddedDoc> {
    vec![
        EmbeddedDoc {
            name: "readme",
            title: "README",
            content: include_str!("../../../README.md"),
        },
        EmbeddedDoc {
            name: "getting-started/installation",
            title: "Installation",
            content: include_str!("../../../docs/src/content/docs/getting-started/installation.md"),
        },
        EmbeddedDoc {
            name: "getting-started/quick-start",
            title: "Quick Start",
            content: include_str!("../../../docs/src/content/docs/getting-started/quick-start.md"),
        },
        EmbeddedDoc {
            name: "getting-started/core-concepts",
            title: "Core Concepts",
            content: include_str!(
                "../../../docs/src/content/docs/getting-started/core-concepts.md"
            ),
        },
        EmbeddedDoc {
            name: "guides/writing-resources",
            title: "Writing Resources",
            content: include_str!("../../../docs/src/content/docs/guides/writing-resources.md"),
        },
        EmbeddedDoc {
            name: "guides/using-modules",
            title: "Using Modules",
            content: include_str!("../../../docs/src/content/docs/guides/using-modules.md"),
        },
        EmbeddedDoc {
            name: "guides/state-management",
            title: "State Management",
            content: include_str!("../../../docs/src/content/docs/guides/state-management.md"),
        },
        EmbeddedDoc {
            name: "guides/functions",
            title: "Functions",
            content: include_str!("../../../docs/src/content/docs/guides/functions.md"),
        },
        EmbeddedDoc {
            name: "guides/for-if-expressions",
            title: "For/If Expressions",
            content: include_str!("../../../docs/src/content/docs/guides/for-if-expressions.md"),
        },
        EmbeddedDoc {
            name: "guides/lsp-setup",
            title: "LSP Setup",
            content: include_str!("../../../docs/src/content/docs/guides/lsp-setup.md"),
        },
        EmbeddedDoc {
            name: "reference/dsl/syntax",
            title: "DSL Syntax",
            content: include_str!("../../../docs/src/content/docs/reference/dsl/syntax.md"),
        },
        EmbeddedDoc {
            name: "reference/dsl/types-and-values",
            title: "Types and Values",
            content: include_str!(
                "../../../docs/src/content/docs/reference/dsl/types-and-values.md"
            ),
        },
        EmbeddedDoc {
            name: "reference/dsl/expressions",
            title: "Expressions",
            content: include_str!("../../../docs/src/content/docs/reference/dsl/expressions.md"),
        },
        EmbeddedDoc {
            name: "reference/dsl/modules",
            title: "Modules Reference",
            content: include_str!("../../../docs/src/content/docs/reference/dsl/modules.md"),
        },
        EmbeddedDoc {
            name: "reference/dsl/built-in-functions",
            title: "Built-in Functions",
            content: include_str!(
                "../../../docs/src/content/docs/reference/dsl/built-in-functions.md"
            ),
        },
        EmbeddedDoc {
            name: "reference/cli/validate",
            title: "CLI: validate",
            content: include_str!("../../../docs/src/content/docs/reference/cli/validate.md"),
        },
        EmbeddedDoc {
            name: "reference/cli/plan",
            title: "CLI: plan",
            content: include_str!("../../../docs/src/content/docs/reference/cli/plan.md"),
        },
        EmbeddedDoc {
            name: "reference/cli/apply",
            title: "CLI: apply",
            content: include_str!("../../../docs/src/content/docs/reference/cli/apply.md"),
        },
        EmbeddedDoc {
            name: "reference/cli/state",
            title: "CLI: state",
            content: include_str!("../../../docs/src/content/docs/reference/cli/state.md"),
        },
        EmbeddedDoc {
            name: "reference/cli/module-info",
            title: "CLI: module info",
            content: include_str!("../../../docs/src/content/docs/reference/cli/module-info.md"),
        },
    ]
}

/// Display the default document (README).
pub fn run_docs_default() -> String {
    run_docs_show("readme").unwrap_or_else(|_| "No default document found.".to_string())
}

/// List all embedded documents.
pub fn run_docs_list() -> String {
    let docs = embedded_docs();
    let lines: Vec<_> = docs
        .iter()
        .map(|doc| format!("  {:<45} {}", doc.name, doc.title))
        .collect();
    format!(
        "Available documents ({}):\n\n{}",
        docs.len(),
        lines.join("\n")
    )
}

/// Show a specific document by name.
pub fn run_docs_show(name: &str) -> Result<String, crate::error::AppError> {
    let docs = embedded_docs();
    docs.into_iter()
        .find(|d| d.name == name)
        .map(|d| d.content.to_string())
        .ok_or_else(|| {
            format!(
                "Document '{}' not found. Use 'carina docs --list' to see available documents.",
                name
            )
            .into()
        })
}

/// Search documents for a query string (case-insensitive).
/// Returns matching lines with document name and line numbers.
pub fn run_docs_search(query: &str) -> String {
    let docs = embedded_docs();
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for doc in &docs {
        for (i, line) in doc.content.lines().enumerate() {
            if line.to_lowercase().contains(&query_lower) {
                results.push(format!("  {}:{}: {}", doc.name, i + 1, line.trim()));
            }
        }
    }

    if results.is_empty() {
        format!("No matches found for '{}'.", query)
    } else {
        let label = if results.len() == 1 {
            "match"
        } else {
            "matches"
        };
        format!(
            "Found {} {} for '{}':\n\n{}",
            results.len(),
            label,
            query,
            results.join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_docs_not_empty() {
        let docs = embedded_docs();
        assert!(!docs.is_empty(), "Should have at least one embedded doc");
    }

    #[test]
    fn test_embedded_docs_have_content() {
        for doc in embedded_docs() {
            assert!(
                !doc.content.is_empty(),
                "Doc '{}' should not be empty",
                doc.name
            );
            assert!(
                !doc.title.is_empty(),
                "Doc '{}' should have a title",
                doc.name
            );
        }
    }

    #[test]
    fn test_embedded_docs_unique_names() {
        let docs = embedded_docs();
        let mut names = std::collections::HashSet::new();
        for doc in &docs {
            assert!(names.insert(doc.name), "Duplicate doc name: {}", doc.name);
        }
    }

    #[test]
    fn test_run_docs_default_shows_readme() {
        let output = run_docs_default();
        assert!(
            output.contains("Carina"),
            "Default doc should contain 'Carina'"
        );
    }

    #[test]
    fn test_run_docs_list() {
        let output = run_docs_list();
        assert!(output.contains("Available documents"));
        assert!(output.contains("readme"));
        assert!(output.contains("reference/dsl/syntax"));
    }

    #[test]
    fn test_run_docs_show_existing() {
        let result = run_docs_show("readme");
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Carina"));
    }

    #[test]
    fn test_run_docs_show_not_found() {
        let result = run_docs_show("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_run_docs_search_found() {
        let output = run_docs_search("provider");
        assert!(output.contains("matches for 'provider'"));
        assert!(!output.contains("No matches found"));
    }

    #[test]
    fn test_run_docs_search_case_insensitive() {
        let output = run_docs_search("CARINA");
        assert!(output.contains("for 'CARINA'"));
    }

    #[test]
    fn test_run_docs_search_no_match() {
        let output = run_docs_search("zzz_nonexistent_xyz_12345");
        assert!(output.contains("No matches found"));
    }
}
