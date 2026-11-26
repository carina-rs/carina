//! The #4591 case table, shared by CLI and LSP diagnostics checks.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    NoLoop,
    Map,
    ValueKeyed,
    Indexed,
    ModuleMap,
    ModuleValueKeyed,
    ModuleIndexed,
}

#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Validates,
    MainFunction,
    SameAsSubscript(&'static str),
    Error(&'static [&'static str]),
    SurfaceErrors {
        cli: &'static [&'static str],
        lsp: &'static [&'static str],
    },
}

#[derive(Debug)]
pub struct Row {
    pub id: &'static str,
    pub kind: Kind,
    pub key: &'static str,
    pub reader: &'static str,
    pub positions: &'static [&'static str],
    pub expected: Outcome,
}

pub const POSITIONS: &[&str] = &[
    "plain",
    "single interpolation",
    "coalesce",
    "if",
    "function argument",
    "multipart interpolation",
    "arithmetic",
    "export",
    "export plain",
    "module argument",
];

pub fn inferred_selector_export(row: &Row, position: &str) -> bool {
    position == "export plain" && matches!(row.id, "2-3" | "2-10" | "2-11" | "2-12" | "2-13")
}

pub fn rows() -> Vec<Row> {
    use Kind::*;
    use Outcome::*;
    let mut rows = Vec::new();
    for kind in [NoLoop, Map] {
        for reader in ["prod", "(prod)"] {
            rows.push(Row {
                id: "2-25",
                kind,
                key: "a",
                reader,
                positions: &["bare function condition"],
                expected: Error(&["if condition must be a Bool value, got: enum identifier prod"]),
            });
        }
    }
    rows.push(Row {
        id: "2-24",
        kind: Map,
        key: "a",
        reader: "children",
        positions: &["unused whole group function", "whole group function"],
        expected: MainFunction,
    });
    for kind in [Map, ValueKeyed] {
        for (id, key, reader, positions, expected) in [
            (
                "2-1",
                "a",
                "children.a.name",
                POSITIONS,
                SameAsSubscript("children['a'].name"),
            ),
            (
                "2-2",
                "a",
                "children.zzz.name",
                POSITIONS,
                Error(&["Undefined identifier `children.zzz`. Did you mean `children.a`?"]),
            ),
            (
                "2-4",
                "a",
                "children.a",
                POSITIONS,
                SameAsSubscript("children['a']"),
            ),
            (
                "2-5",
                "a",
                "children.a.tags['k']",
                POSITIONS,
                SameAsSubscript("children['a'].tags['k']"),
            ),
            (
                "2-6",
                "a",
                "children.a.tags",
                &["iterable"][..],
                SameAsSubscript("children['a'].tags"),
            ),
            (
                "2-6a",
                "a",
                "children.a.tags",
                &["simple iterable"][..],
                SameAsSubscript("children['a'].tags"),
            ),
            (
                "2-19",
                "a",
                "children.a.name",
                &[
                    "function body",
                    "parameter default",
                    "unused function body",
                    "unused parameter default",
                ][..],
                MainFunction,
            ),
            (
                "2-7",
                "name",
                "children.name.name",
                POSITIONS,
                SameAsSubscript("children['name'].name"),
            ),
            (
                "2-8",
                "a1",
                "children.a1.name",
                POSITIONS,
                SameAsSubscript("children['a1'].name"),
            ),
            (
                "2-11",
                "0",
                "children.0.name",
                POSITIONS,
                Error(&["cannot apply access .0 to keyed loop group"]),
            ),
            (
                "2-13",
                "3rd",
                "children.3rd.name",
                POSITIONS,
                Error(&["cannot apply access .3rd to keyed loop group"]),
            ),
            (
                "2-15",
                "a.b",
                "children.a.b.name",
                POSITIONS,
                Error(&["Undefined identifier `children.a`. Did you mean `children`?"]),
            ),
        ] {
            rows.push(Row {
                id,
                kind,
                key,
                reader,
                positions,
                expected,
            });
        }
        // Measured subtraction diagnostics (2-14), including the scope names
        // introduced by each sink. The missing bare child is not diagnosed as
        // a missing-key read; the arithmetic checker sees its resource type.
        for (positions, expected) in [
            (
                &POSITIONS[..7],
                Error(&[
                    "Undefined identifier `key`. In-scope names: children, children['my-key'], items, other (let / upstream_state / read / module / function / for / fn / arguments)",
                    "operator '-': left operand must be Int, got mock.test.resource",
                ]),
            ),
            (
                &["export"][..],
                Error(&[
                    "Undefined identifier `key`. In-scope names: children, children['my-key'], items (let / upstream_state / read / module / function / for / fn / arguments)",
                    "operator '-': left operand must be Int, got mock.test.resource",
                ]),
            ),
            (
                &["export plain"][..],
                Error(&[
                    "Undefined identifier `key`. In-scope names: children, children['my-key'], items (let / upstream_state / read / module / function / for / fn / arguments)",
                    "./exports.crn:1:11: export 'out': type annotation required: operator '-': left operand must be Int, got mock.test.resource",
                    "operator '-': left operand must be Int, got mock.test.resource",
                ]),
            ),
            (
                &["module argument"][..],
                Error(&[
                    "Undefined identifier `key`. In-scope names: children, children['my-key'], consume, items (let / upstream_state / read / module / function / for / fn / arguments)",
                    "operator '-': left operand must be Int, got mock.test.resource",
                ]),
            ),
        ] {
            rows.push(Row {
                id: "2-14",
                kind,
                key: "my-key",
                reader: "children.my-key.name",
                positions,
                expected: SurfaceErrors {
                    cli: match expected {
                        Error(errors) => errors,
                        _ => unreachable!("subtraction errors"),
                    },
                    lsp: &[
                        "Undefined resource: 'key'. Define it with 'let key = aws...'",
                        "operator '-': left operand must be Int, got mock.test.resource",
                    ],
                },
            });
        }
        rows.push(Row {
            id: "2-1",
            kind,
            key: "a",
            reader: "children.a.name",
            positions: &["plain"],
            expected: Validates,
        });
    }
    for reader in ["children.a.name", "children.zzz.name"] {
        rows.push(Row {
            id: "2-3",
            kind: Indexed,
            key: "a",
            reader,
            positions: POSITIONS,
            expected: Error(if reader == "children.a.name" {
                &["cannot apply access .a to list(mock.test.resource)"]
            } else {
                &["cannot apply access .zzz to list(mock.test.resource)"]
            }),
        });
    }
    rows.push(Row {
        id: "2-19",
        kind: Indexed,
        key: "a",
        reader: "children.a.name",
        positions: &[
            "function body",
            "parameter default",
            "unused function body",
            "unused parameter default",
        ],
        expected: MainFunction,
    });
    for kind in [Map, ValueKeyed, Indexed] {
        rows.push(Row {
            id: if kind == Indexed { "2-19" } else { "2-22" },
            kind,
            key: "a",
            reader: "children.a.name",
            positions: &["deferred function body", "deferred parameter default"],
            expected: MainFunction,
        });
        rows.push(Row {
            id: "2-20",
            kind,
            key: "a",
            reader: "nope.x",
            positions: &["unrelated reference"],
            expected: SurfaceErrors {
                cli: if kind == Indexed {
                    &["Undefined identifier `nope`. In-scope names: children, children[0], items, o (let / upstream_state / read / module / function / for / fn / arguments)"]
                } else {
                    &["Undefined identifier `nope`. In-scope names: children, children.a, items, o (let / upstream_state / read / module / function / for / fn / arguments)"]
                },
                lsp: &["Undefined resource: 'nope'. Define it with 'let nope = aws...'"],
            },
        });
    }
    rows.push(Row {
        id: "2-23",
        kind: NoLoop,
        key: "a",
        reader: "r.name + 1",
        positions: &["unrelated export failure"],
        expected: SurfaceErrors {
            cli: &[
                "./exports.crn:1:11: export 'out': type annotation required: operator '+': left operand must be Int, got String",
                "operator '+': left operand must be Int, got String",
            ],
            lsp: &[
                "operator '+': left operand must be Int, got String",
                "export 'out': type annotation required: operator '+': left operand must be Int, got String",
            ],
        },
    });
    for kind in [NoLoop, Map, ValueKeyed, Indexed] {
        rows.push(Row {
            id: "2-21",
            kind,
            key: "a",
            reader: "foo.bar",
            positions: &["unrelated reference"],
            expected: SurfaceErrors {
                cli: &["Undefined identifier `foo`. Did you mean `o`?"],
                lsp: &["Undefined resource: 'foo'. Define it with 'let foo = aws...'"],
            },
        });
    }
    for kind in [Map, ValueKeyed, Indexed] {
        rows.push(Row {
            id: "2-19",
            kind,
            key: "a",
            reader: "children.a.name ?? 'p'",
            positions: &["function body", "parameter default"],
            expected: MainFunction,
        });
    }
    rows.push(Row {
        id: "2-12",
        kind: Indexed,
        key: "a",
        reader: "children.0.name",
        positions: POSITIONS,
        expected: Error(&["cannot apply access .0 to list(mock.test.resource)"]),
    });
    for kind in [ModuleMap, ModuleValueKeyed] {
        rows.push(Row {
            id: "2-9",
            kind,
            key: "a",
            reader: "mods.a.x",
            positions: POSITIONS,
            expected: SameAsSubscript("mods['a'].x"),
        });
    }
    rows.push(Row {
        id: "2-10",
        kind: ModuleIndexed,
        key: "a",
        reader: "mods.a.x",
        positions: POSITIONS,
        expected: Error(&["cannot apply access .a to list(value-dependent expression)"]),
    });
    rows
}

/// Full records measured on main 2ab61c021, including each surface's locations.
pub fn main_function_record(row: &Row, position: &str) -> &'static serde_json::Value {
    static RECORDED: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let recorded = RECORDED.get_or_init(|| {
        serde_json::from_str(include_str!("../fixtures/loop_dot_function_main.json")).unwrap()
    });
    let key = if row.id == "2-24" {
        format!(
            "whole-group-{}",
            if position == "whole group function" {
                "called"
            } else {
                "uncalled"
            }
        )
    } else {
        let kind = match row.kind {
            Kind::Map => "map",
            Kind::ValueKeyed => "value-keyed",
            Kind::Indexed => "indexed",
            _ => unreachable!("function case loop kind"),
        };
        format!(
            "{kind}-{}{}",
            position.replace(' ', "-"),
            if row.reader.ends_with("?? 'p'") {
                "-coalesce"
            } else {
                ""
            }
        )
    };
    let expected = &recorded["cases"][&key];
    assert!(!expected.is_null(), "missing main measurement for {key}");
    expected
}

pub fn write(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

/// Instantiate the same loop in a module with declarations and readers in
/// separate files, so validation also visits the expanded resource values.
pub fn module_fixture(root: &Path, row: &Row) {
    fixture(root, row);
    let declarations = std::fs::read_to_string(root.join("main.crn")).unwrap();
    write(root, "internal/loops.crn", &declarations);
    write(
        root,
        "internal/arguments.crn",
        "arguments { value: String }\n",
    );
    write(
        root,
        "main.crn",
        "let internal = use { source = './internal' }\nlet instance = internal { value = 'x' }\n",
    );
}

pub fn module_reader(root: &Path, position: &str, expression: &str) -> String {
    let module = root.join("internal");
    let owner = reader(&module, position, expression);
    write(&module, "exports.crn", "");
    format!("internal/{owner}")
}

pub fn fixture(root: &Path, row: &Row) {
    use Kind::*;
    let key = row.key;
    let (items, iterable) = match row.kind {
        NoLoop => ("{}".to_owned(), ""),
        Map | ModuleMap => (format!("{{ '{key}' = 'x' }}"), "k, v in items"),
        ValueKeyed | ModuleValueKeyed => (format!("['{key}']"), "v in items"),
        Indexed | ModuleIndexed => ("['a']".to_owned(), "(i, v) in items"),
    };
    let main = match (row.id, row.kind) {
        ("2-25", kind) => format!(
            "let prod = true\n{}",
            if kind == NoLoop {
                ""
            } else {
                "let children = for k, v in ({ a = 'x' }) { mock.test.resource { name = k } }\n"
            },
        ),
        ("2-24", _) => {
            "let children = for k, v in ({ a = 'x' }) { mock.test.resource { name = k } }\n"
                .to_owned()
        }
        (_, NoLoop) => format!("let items = {items}\n"),
        (_, Map | ValueKeyed | Indexed) => format!(
            "let items = {items}\nlet children = for {iterable} {{ mock.test.resource {{ name = v tags = {{ k = 'tag' }} }} }}\n"
        ),
        (_, ModuleMap | ModuleValueKeyed | ModuleIndexed) => format!(
            "let items = {items}\nlet child = use {{ source = './child' }}\nlet mods = for {iterable} {{ child {{ value = v }} }}\n"
        ),
    };
    for (name, body) in [
        ("main.crn", main.as_str()),
        ("other.crn", ""),
        ("callee.crn", ""),
        ("exports.crn", "exports {}\n"),
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
        ("child/arguments.crn", "arguments { value: String }\n"),
        ("child/main.crn", ""),
        ("child/attributes.crn", "attributes { x: String = value }\n"),
        ("consumer/arguments.crn", "arguments { value: String }\n"),
        (
            "consumer/main.crn",
            "mock.test.resource { name = 'consumer' comment = value }\n",
        ),
    ] {
        write(root, name, body);
    }
}

pub fn reader(root: &Path, position: &str, reader: &str) -> &'static str {
    if position == "bare function condition" {
        let source = std::fs::read_to_string(root.join("main.crn")).unwrap();
        let declarations = source.split("fn mode").next().unwrap();
        write(
            root,
            "main.crn",
            &format!(
                "{declarations}fn mode(): String {{ if {reader} {{ 'a' }} else {{ 'b' }} }}\n"
            ),
        );
        return "main.crn";
    }
    if matches!(
        position,
        "unused whole group function" | "whole group function"
    ) {
        let source = std::fs::read_to_string(root.join("main.crn")).unwrap();
        let declaration = source.lines().next().unwrap();
        let call = if position == "whole group function" {
            "let r = mock.test.resource { name = \"${n()}\" }\n"
        } else {
            ""
        };
        write(
            root,
            "main.crn",
            &format!("{declaration}\nfn n(): Int {{ length({reader}) }}\n{call}"),
        );
        return "main.crn";
    }
    let expression = match position {
        "single interpolation" => format!("\"${{{reader}}}\""),
        "coalesce" | "export" | "unrelated reference" => format!("{reader} ?? 'p'"),
        "if" => format!("if true {{ {reader} }} else {{ 'p' }}"),
        "function argument" => format!("upper({reader})"),
        "multipart interpolation" => format!("\"x-${{{reader}}}\""),
        "arithmetic" => format!("\"${{length({reader}) + 1}}\""),
        _ => reader.to_owned(),
    };
    let (other, exports, owner) = match position {
        "deferred function body" | "deferred parameter default" => {
            write(root, "callee.crn", "fn g(): String { 'gee' }\n");
            let function = if position == "deferred function body" {
                format!("fn selected(x: String): String {{ \"${{x}}-${{{expression}}}\" }}")
            } else {
                format!(
                    "fn selected(x: String, value: String = {expression}): String {{ \"${{x}}-${{value}}\" }}"
                )
            };
            (
                format!(
                    "{function}\nlet other = mock.test.resource {{ name = 'other' comment = selected(g()) }}\n"
                ),
                "exports {}\n".to_owned(),
                "other.crn",
            )
        }
        "unrelated export failure" => (
            "let r = mock.test.resource { name = 'r' }\n".to_owned(),
            format!("exports {{ out = {expression} }}\n"),
            "exports.crn",
        ),
        "function body"
        | "parameter default"
        | "unused function body"
        | "unused parameter default" => {
            let function = if position.ends_with("function body") {
                format!("fn selected(): String {{ {expression} }}")
            } else {
                format!("fn selected(value: String = {expression}): String {{ value }}")
            };
            let other = if position.starts_with("unused") {
                format!("{function}\n")
            } else {
                format!(
                    "{function}\nlet other = mock.test.resource {{ name = 'other' comment = selected() }}\n"
                )
            };
            (other, "exports {}\n".to_owned(), "other.crn")
        }
        "unrelated reference" => (
            format!("let o = mock.test.resource {{ name = 'other' comment = {expression} }}\n"),
            "exports {}\n".to_owned(),
            "other.crn",
        ),
        "export" => (
            String::new(),
            format!("exports {{ out: String = {expression} }}\n"),
            "exports.crn",
        ),
        "export plain" => (
            String::new(),
            format!("exports {{ out = {expression} }}\n"),
            "exports.crn",
        ),
        // Call and anonymous-loop identities include their enclosing spans.
        // Keep those spans identical while comparing the full plan JSON.
        "module argument" => (
            format!(
                "let consume = use {{ source = './consumer' }}\nconsume {{ value = {expression:<80} }}\n"
            ),
            "exports {}\n".to_owned(),
            "other.crn",
        ),
        "iterable" => (
            format!("for k, t in {expression:<80} {{ mock.test.resource {{ name = t }} }}\n"),
            "exports {}\n".to_owned(),
            "other.crn",
        ),
        "simple iterable" => (
            format!("for t in {expression:<80} {{ mock.test.resource {{ name = t }} }}\n"),
            "exports {}\n".to_owned(),
            "other.crn",
        ),
        _ => (
            format!("let other = mock.test.resource {{ name = 'other' comment = {expression} }}\n"),
            "exports { out: String = other.name }\n".to_owned(),
            "other.crn",
        ),
    };
    write(root, "other.crn", &other);
    write(root, "exports.crn", &exports);
    owner
}
