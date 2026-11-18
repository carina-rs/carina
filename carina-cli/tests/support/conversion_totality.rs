//! Deterministic placement and transport fixtures, independent of completion.
#[path = "conversion_factory.rs"]
mod factory;
pub use factory::factories;
use std::{collections::BTreeMap, path::Path};

#[derive(Debug)]
pub struct Fixture {
    pub files: BTreeMap<String, String>,
}
impl Fixture {
    pub fn source(source: String) -> Self {
        Self {
            files: BTreeMap::from([
                ("providers.crn".into(), "provider mock {}\n".into()),
                (
                    "backend.crn".into(),
                    "backend local { path = 'carina.state.json' }\n".into(),
                ),
                ("main.crn".into(), source),
            ]),
        }
    }
    pub fn write(&self, directory: &Path) {
        for (file, source) in &self.files {
            let path = directory.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Position {
    Root,
    Parens,
    ListItem,
    MapValue,
    StructField,
    StructBlock,
    IfBranch,
    Coalesce,
    Definition,
    Alias,
    AliasForward,
    SequenceResult,
    Union,
    Secret,
    DataSource,
}
pub const POSITIONS: [Position; 15] = [
    Position::Root,
    Position::Parens,
    Position::ListItem,
    Position::MapValue,
    Position::StructField,
    Position::StructBlock,
    Position::IfBranch,
    Position::Coalesce,
    Position::Definition,
    Position::Alias,
    Position::AliasForward,
    Position::SequenceResult,
    Position::Union,
    Position::Secret,
    Position::DataSource,
];
pub const TRANSPORT_EXAMPLES: &[(&str, Position)] = &[
    ("Parentheses", Position::Parens),
    ("IfValue", Position::IfBranch),
    ("CoalesceValue", Position::Coalesce),
    ("ListElement", Position::ListItem),
    ("MapValue", Position::MapValue),
    ("StructFieldValue", Position::StructBlock),
    ("DefinitionValue", Position::Definition),
    ("AliasForward", Position::AliasForward),
    ("AttributeSlot", Position::Root),
    ("SequenceResult", Position::SequenceResult),
];
impl Position {
    pub fn label(self) -> &'static str {
        match self {
            Self::Root => "convert-attribute-slot",
            Self::Parens => "parentheses",
            Self::ListItem => "list-element",
            Self::MapValue => "map-value",
            Self::StructField => "struct-field-value",
            Self::StructBlock => "nested-block-value",
            Self::IfBranch => "if-value",
            Self::Coalesce => "coalesce-value",
            Self::Definition => "definition-value",
            Self::Alias => "binding-forward",
            Self::AliasForward => "alias-forward",
            Self::SequenceResult => "sequence-result",
            Self::Union => "union",
            Self::Secret => "secret-source",
            Self::DataSource => "data-source-slot",
        }
    }
}
pub fn fixture(position: Position, known_schema: bool, invalid: bool, converted: bool) -> Fixture {
    let value = if invalid { "42" } else { "'ok'" };
    let cast = |value: &str| {
        if converted {
            format!("{value} as mock.Name")
        } else {
            value.into()
        }
    };
    let expression = cast(value);
    let (prefix, attributes) = match position {
        Position::Root | Position::DataSource => (String::new(), format!("attr = {expression}")),
        Position::Parens => (String::new(), format!("attr = (({expression}))")),
        Position::ListItem => (String::new(), format!("items = [{expression}]")),
        Position::MapValue => (String::new(), format!("values = {{ key = {expression} }}")),
        Position::StructField => (
            String::new(),
            format!("config = {{ value = {expression} }}"),
        ),
        Position::StructBlock => (String::new(), format!("entry {{ value = {expression} }}")),
        Position::IfBranch => (
            String::new(),
            format!(
                "attr = if true {{ {expression} }} else {{ {} }}",
                cast("'ok'")
            ),
        ),
        Position::Coalesce => (
            format!("let converted = {expression}\n"),
            "attr = converted ?? converted".into(),
        ),
        Position::Definition => (
            String::new(),
            format!("let local = {expression}\nattr = local"),
        ),
        Position::Alias => (
            format!("let converted = {expression}\n"),
            "attr = converted".into(),
        ),
        Position::AliasForward => (
            format!("let converted = {expression}\nlet alias = converted\n"),
            "attr = alias".into(),
        ),
        Position::SequenceResult => (
            String::new(),
            format!(
                "attr = if true {{ let result = {expression}\nresult }} else {{ {} }}",
                cast("'ok'")
            ),
        ),
        Position::Union => (String::new(), format!("choice = {expression}")),
        Position::Secret => (
            String::new(),
            format!("attr = {}", cast(&format!("(secret({value}))"))),
        ),
    };
    let kind = if known_schema {
        match position {
            Position::DataSource => "read mock.Query",
            Position::Root
            | Position::Parens
            | Position::ListItem
            | Position::MapValue
            | Position::StructField
            | Position::StructBlock
            | Position::IfBranch
            | Position::Coalesce
            | Position::Definition
            | Position::Alias
            | Position::AliasForward
            | Position::SequenceResult
            | Position::Union
            | Position::Secret => "mock.Sink",
        }
    } else {
        "mock.NotLoaded"
    };
    Fixture::source(format!("{prefix}let sink = {kind} {{ {attributes} }}\n"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evaluation {
    Live,
    DiscardedIf,
    DiscardedCoalesce,
    NeverExpanded,
}
pub const EVALUATIONS: [Evaluation; 4] = [
    Evaluation::Live,
    Evaluation::DiscardedIf,
    Evaluation::DiscardedCoalesce,
    Evaluation::NeverExpanded,
];
impl Evaluation {
    fn value(self, converted: bool, operand: &str) -> String {
        let expression = if converted {
            format!("({operand}) as unloaded.Name")
        } else {
            operand.into()
        };
        match self {
            Self::Live | Self::NeverExpanded => expression,
            Self::DiscardedIf => format!("if true {{ {operand} }} else {{ {expression} }}"),
            Self::DiscardedCoalesce => format!("({operand}) ?? ({expression})"),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Placement {
    Transport,
    Computed,
    Boundary,
}
#[derive(Debug)]
pub struct Context {
    pub label: &'static str,
    pub rules: &'static [&'static str],
    pub placement: Placement,
    pub prefix: &'static str,
    pub expression: &'static str,
    pub structural: bool,
    pub statement: bool,
    pub operand: &'static str,
    pub parser_rejection: Option<(&'static [Evaluation], &'static str)>,
}
impl Context {
    pub fn parser_rejection(&self, evaluation: Evaluation) -> Option<&'static str> {
        self.parser_rejection
            .and_then(|(states, reason)| states.contains(&evaluation).then_some(reason))
    }
    pub fn fixture(
        &self,
        evaluation: Evaluation,
        converted: bool,
    ) -> Result<Fixture, &'static str> {
        if matches!(evaluation, Evaluation::NeverExpanded) && !self.structural {
            return Err("this declaration cannot be the resource/module tail of a structural body");
        }
        let expression = self
            .expression
            .replace("VALUE", &evaluation.value(converted, self.operand));
        let source = if matches!(evaluation, Evaluation::NeverExpanded) {
            format!(
                "{}\nfor item in [] {{ mock.Sink {{ attr = {expression} }} }}\n",
                self.prefix
            )
        } else if self.statement {
            format!("{}\n{expression}\n", self.prefix)
        } else {
            format!("{}\nlet unused = {expression}\n", self.prefix)
        };
        Ok(Fixture::source(source))
    }
}

pub const CONTEXTS: &[Context] = &[
    Context {
        label: "parentheses",
        rules: &["expression", "primary"],
        placement: Placement::Transport,
        prefix: "",
        expression: "((VALUE))",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "convert",
        rules: &["postfix_expr", "as_keyword", "conversion_type_path"],
        placement: Placement::Transport,
        prefix: "",
        expression: "VALUE",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "coalesce",
        rules: &["coalesce_expr"],
        placement: Placement::Transport,
        prefix: "",
        expression: "(VALUE) ?? 'ok'",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "list",
        rules: &["list"],
        placement: Placement::Transport,
        prefix: "",
        expression: "[VALUE]",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "map-value",
        rules: &["map", "map_entry"],
        placement: Placement::Transport,
        prefix: "",
        expression: "{ key = VALUE }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "nested-field",
        rules: &["nested_block"],
        placement: Placement::Transport,
        prefix: "",
        expression: "{ field { key = VALUE } }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "if-value",
        rules: &["if_expr", "if_body", "else_clause"],
        placement: Placement::Transport,
        prefix: "",
        expression: "if true { VALUE } else { 'ok' }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "let-value",
        rules: &["let_binding", "let_head", "discard_pattern"],
        placement: Placement::Transport,
        prefix: "",
        expression: "let unused = VALUE",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "local-value",
        rules: &["local_binding", "block_content", "attribute"],
        placement: Placement::Transport,
        prefix: "",
        expression: "mock.Sink { let local = VALUE\n attr = local }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "for-local",
        rules: &["for_local_binding", "for_body"],
        placement: Placement::Transport,
        prefix: "",
        expression: "for item in [] { let local = VALUE\n mock.Sink { attr = local } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "if-local",
        rules: &["if_local_binding"],
        placement: Placement::Transport,
        prefix: "",
        expression: "if true { let local = VALUE\n mock.Sink { attr = local } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "slot",
        rules: &["resource_expr", "anonymous_resource"],
        placement: Placement::Transport,
        prefix: "",
        expression: "mock.Sink { attr = VALUE }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "data-slot",
        rules: &["read_resource_expr"],
        placement: Placement::Transport,
        prefix: "",
        expression: "read mock.Query { attr = VALUE }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "binding-forward",
        rules: &["variable_ref"],
        placement: Placement::Transport,
        prefix: "",
        expression: "let converted = VALUE\nlet alias = converted",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "arithmetic-add",
        rules: &["additive_expr", "add_op", "plus_op", "minus_op"],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE) + 1",
        structural: true,
        statement: false,
        operand: "1",
        parser_rejection: None,
    },
    Context {
        label: "arithmetic-multiply",
        rules: &[
            "multiplicative_expr",
            "multiplicative_op",
            "multiply_op",
            "divide_op",
            "remainder_op",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE) * 2",
        structural: true,
        statement: false,
        operand: "1",
        parser_rejection: None,
    },
    Context {
        label: "builtin-argument",
        rules: &["function_call"],
        placement: Placement::Computed,
        prefix: "",
        expression: "upper(VALUE)",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "function-argument",
        rules: &[],
        placement: Placement::Computed,
        prefix: "fn echo(value: String): String { value }",
        expression: "echo(VALUE)",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "interpolation",
        rules: &[
            "interpolation",
            "interpolation_expr",
            "double_quoted_string",
            "string",
            "string_part",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "\"${VALUE}\"",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "pipe",
        rules: &["pipe_expr"],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE) |> upper()",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "compose",
        rules: &["compose_expr"],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE) >> join('-')",
        structural: true,
        statement: false,
        operand: "split(',')",
        parser_rejection: Some((
            &[Evaluation::DiscardedIf, Evaluation::DiscardedCoalesce],
            "closures are not data in if/?? operands: parsing fails before a judgement or Value exists",
        )),
    },
    Context {
        label: "index",
        rules: &["index_access", "subscripted_id"],
        placement: Placement::Computed,
        prefix: "let values = ['ok']",
        expression: "values[VALUE]",
        structural: true,
        statement: false,
        operand: "0",
        parser_rejection: None,
    },
    Context {
        label: "access-base",
        rules: &[
            "field_access",
            "namespaced_id",
            "reference_prefix",
            "reference_field",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE).field",
        structural: true,
        statement: false,
        operand: "{ field = 'ok' }",
        parser_rejection: None,
    },
    Context {
        label: "literal-access",
        rules: &[],
        placement: Placement::Computed,
        prefix: "",
        expression: "let m = { a = VALUE }\nlet unused = m.a",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "map-key",
        rules: &[],
        placement: Placement::Computed,
        prefix: "",
        expression: "{ \"${VALUE}\" = 'ok' }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "condition",
        rules: &["if_head"],
        placement: Placement::Computed,
        prefix: "",
        expression: "if (VALUE) { 'ok' } else { 'ok' }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "for-iterable",
        rules: &[
            "for_expr",
            "for_head",
            "for_iterable",
            "for_iterable_primary",
            "for_binding",
            "for_simple_binding",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "for n in [VALUE] { mock.Sink { attr = n } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "for-iterable-unused",
        rules: &[],
        placement: Placement::Computed,
        prefix: "",
        expression: "for n in [VALUE] { mock.Sink { attr = 'ok' } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "for-indexed",
        rules: &["for_indexed_binding"],
        placement: Placement::Computed,
        prefix: "",
        expression: "for (i, n) in [VALUE] { mock.Sink { attr = n } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "for-map",
        rules: &["for_map_binding"],
        placement: Placement::Computed,
        prefix: "",
        expression: "for key, n in ({ a = VALUE }) { mock.Sink { attr = n } }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "secret",
        rules: &[],
        placement: Placement::Computed,
        prefix: "",
        expression: "secret(VALUE)",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "nested-convert",
        rules: &[],
        placement: Placement::Computed,
        prefix: "",
        expression: "(VALUE) as mock.Name",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "function-body",
        rules: &["fn_def", "fn_body"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "fn unused(): String { VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "function-default",
        rules: &["fn_params", "fn_param"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "fn unused(value: String = VALUE): String { value }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "function-local",
        rules: &["fn_local_let"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "fn unused(): String { let local = VALUE\n local }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "module-default",
        rules: &[
            "arguments_block",
            "arguments_param",
            "arguments_param_block",
            "arguments_param_attr",
            "arg_default_attr",
        ],
        placement: Placement::Boundary,
        prefix: "",
        expression: "arguments { value: String = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "module-argument",
        rules: &["module_call", "module_call_arg"],
        placement: Placement::Boundary,
        prefix: "let child = use { source = './child' }",
        expression: "child { value = VALUE }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "output",
        rules: &["attributes_block", "attributes_param"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "attributes { out = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "export",
        rules: &["exports_block", "exports_param"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "exports { out = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "provider",
        rules: &["provider_block", "provider_expr"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "provider mock { endpoint = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "backend",
        rules: &["backend_block"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "backend local { path = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "upstream-source",
        rules: &["upstream_state_expr"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "upstream_state { source = VALUE }",
        structural: true,
        statement: false,
        operand: "'ok'",
        parser_rejection: Some((
            &[
                Evaluation::Live,
                Evaluation::DiscardedIf,
                Evaluation::DiscardedCoalesce,
            ],
            "upstream source must be a literal: parsing fails before a judgement or Value exists",
        )),
    },
    Context {
        label: "use-source",
        rules: &["use_expr"],
        placement: Placement::Boundary,
        prefix: "",
        expression: "let child = use { source = VALUE }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
    Context {
        label: "directives",
        rules: &[],
        placement: Placement::Boundary,
        prefix: "let target = mock.Sink { attr = 'ok' }",
        expression: "mock.Sink { directives { depends_on = [VALUE] } attr = 'ok' }",
        structural: true,
        statement: false,
        operand: "target",
        parser_rejection: Some((
            &[Evaluation::DiscardedIf, Evaluation::DiscardedCoalesce],
            "resource handles are not data in if/?? operands: parsing fails before a judgement or Value exists",
        )),
    },
    Context {
        label: "wait-dependencies",
        rules: &["wait_depends_on_attr"],
        placement: Placement::Computed,
        prefix: "let target = mock.Sink { attr = 'ok' }",
        expression: "wait target { until = target.attr == 'ok' depends_on = [VALUE] }",
        structural: true,
        statement: false,
        operand: "target",
        parser_rejection: Some((
            &[Evaluation::DiscardedIf, Evaluation::DiscardedCoalesce],
            "resource handles are not data in if/?? operands: parsing fails before a judgement or Value exists",
        )),
    },
    Context {
        label: "wait-until-alias",
        rules: &["wait_expr", "wait_attr", "wait_until_attr"],
        placement: Placement::Computed,
        prefix: "let target = mock.Sink { attr = 'ok' }",
        expression: "let converted = VALUE\nlet awaited = wait target { until = converted == true }",
        structural: false,
        statement: true,
        operand: "true",
        parser_rejection: Some((
            &[
                Evaluation::Live,
                Evaluation::DiscardedIf,
                Evaluation::DiscardedCoalesce,
            ],
            "wait predicates require the target attribute on the left and a closed literal/enum on the right; a bare alias is rejected before Judge",
        )),
    },
    Context {
        label: "validation-condition-alias",
        rules: &[
            "arg_validation_block",
            "validation_block_attr",
            "validation_condition_attr",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "let converted = VALUE\narguments { input: Bool { validation { condition = converted error_message = 'bad' } } }",
        structural: false,
        statement: true,
        operand: "true",
        parser_rejection: None,
    },
    Context {
        label: "require-alias",
        rules: &[
            "require_head",
            "require_condition",
            "require_statement",
            "validate_expr",
            "validate_or_expr",
            "validate_and_expr",
            "validate_not_expr",
            "validate_comparison",
            "validate_additive_expr",
            "validate_multiplicative_expr",
            "validate_primary",
            "validate_function_call",
        ],
        placement: Placement::Computed,
        prefix: "",
        expression: "let converted = VALUE\narguments { require converted == 'ok', 'guard' }",
        structural: false,
        statement: true,
        operand: "'ok'",
        parser_rejection: None,
    },
];

/// Closed grammar audit: entries have no independently admissible value child.
pub const NON_CONTEXT_RULES: &[(&str, &str)] = &[
    (
        "file",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "statement",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "wait_timeout_attr",
        "validate-expression/condition grammar has no checked-conversion suffix",
    ),
    (
        "arg_description_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "validation_error_message_attr",
        "validate-expression/condition grammar has no checked-conversion suffix",
    ),
    (
        "compare_op",
        "validate-expression/condition grammar has no checked-conversion suffix",
    ),
    (
        "coalesce_separator",
        "diagnostic separator around the coalesce operator has no independently admissible value child",
    ),
    (
        "coalesce_token",
        "coalesce operator token has no independently admissible value child",
    ),
    (
        "null_literal",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_expr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_expr_atom",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_simple",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_generic",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_ref",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "resource_type_path",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_struct",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "struct_field_list",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "struct_field",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "type_string_literal",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "resource_address",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "import_state_block",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "import_state_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "import_to_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "import_id_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "removed_block",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "removed_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "moved_block",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "moved_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "moved_from_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "moved_to_attr",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "use_keyword",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "identifier",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "numeric_tail",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "boolean",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "float",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "number",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "duration_literal",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "duration_unit",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "inline_ws",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "string_literal",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "string_char",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "single_quoted_string",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "single_quoted_content",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "single_quoted_char",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "WHITESPACE",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "COMMENT",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "line_comment",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
    (
        "block_comment",
        "lexical/type/address token or structural envelope; its expression children are classified separately",
    ),
];

/// Static source-type ownership examples; both forms have a wrong source for
/// mock.Name, and the converted form names the distinct loaded mock.Other.
#[derive(Clone, Copy, Debug)]
pub enum OwnerPosition {
    BareBinding,
    AliasOperand,
    ModuleInstance,
    KnownIterableRead,
    DeferredManaged,
    DeferredReadGuard,
    NestedReference,
}
pub const OWNER_POSITIONS: [OwnerPosition; 7] = [
    OwnerPosition::BareBinding,
    OwnerPosition::AliasOperand,
    OwnerPosition::ModuleInstance,
    OwnerPosition::KnownIterableRead,
    OwnerPosition::DeferredManaged,
    OwnerPosition::DeferredReadGuard,
    OwnerPosition::NestedReference,
];
impl OwnerPosition {
    pub fn label(self) -> &'static str {
        match self {
            Self::BareBinding => "bare-binding-operand",
            Self::AliasOperand => "alias-operand",
            Self::ModuleInstance => "module-instance",
            Self::KnownIterableRead => "known-iterable-read",
            Self::DeferredManaged => "managed-deferred-template",
            Self::DeferredReadGuard => "deferred-read-ordinary-guard",
            Self::NestedReference => "nested-reference-leaf",
        }
    }
    pub fn fixture(self, converted: bool) -> Fixture {
        let cast = |operand: &str| {
            if converted {
                format!("{operand} as mock.Other")
            } else {
                operand.to_owned()
            }
        };
        let source = match self {
            Self::BareBinding => {
                format!("let value = 7\nmock.Sink {{ attr = {} }}\n", cast("value"))
            }
            Self::AliasOperand => format!(
                "let value = 7\nlet alias = {}\nmock.Sink {{ attr = alias }}\n",
                cast("value")
            ),
            Self::ModuleInstance => {
                "let child = use { source = './child' }\nlet instance = child { value = 7 }\n"
                    .into()
            }
            Self::KnownIterableRead => format!(
                "let results = for n in [7] {{ read mock.Query {{ attr = {} }} }}\n",
                cast("n")
            ),
            Self::DeferredManaged => format!(
                "let p = mock.Source {{}}\nlet results = for n in p.items {{ mock.Sink {{ attr = {} }} }}\n",
                cast("7")
            ),
            Self::DeferredReadGuard => format!(
                "let p = mock.Source {{}}\nlet results = for n in p.items {{ read mock.Query {{ attr = {} }} }}\n",
                cast("7")
            ),
            Self::NestedReference => format!(
                "let p = mock.Source {{ number = 7 }}\nmock.Sink {{ config = {{ value = {} }} }}\n",
                cast("p.number")
            ),
        };
        let mut fixture = Fixture::source(source);
        if matches!(self, Self::ModuleInstance) {
            fixture.files.insert(
                "child/main.crn".into(),
                format!(
                    "arguments {{ value: Int }}\nmock.Sink {{ attr = {} }}\n",
                    cast("value")
                ),
            );
        }
        fixture
    }
}
