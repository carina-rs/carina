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

pub fn rows() -> Vec<Row> {
    use Kind::*;
    use Outcome::*;
    let mut rows = Vec::new();
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
                SameAsSubscript("children['a'].name"),
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
        expected: Error(&["cannot apply access .a to list(mock.test.resource)"]),
    });
    for kind in [Map, ValueKeyed, Indexed] {
        rows.push(Row {
            id: if kind == Indexed { "2-19" } else { "2-22" },
            kind,
            key: "a",
            reader: "children.a.name",
            positions: &["deferred function body", "deferred parameter default"],
            expected: if kind == Indexed {
                Error(&["cannot apply access .a to list(mock.test.resource)"])
            } else {
                SameAsSubscript("children['a'].name")
            },
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
            expected: if kind == Indexed {
                Error(&["cannot apply access .a to list(mock.test.resource)"])
            } else {
                SameAsSubscript("children['a'].name ?? 'p'")
            },
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

pub fn write(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
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
    let main = match row.kind {
        NoLoop => format!("let items = {items}\n"),
        Map | ValueKeyed | Indexed => format!(
            "let items = {items}\nlet children = for {iterable} {{ mock.test.resource {{ name = v tags = {{ k = 'tag' }} }} }}\n"
        ),
        ModuleMap | ModuleValueKeyed | ModuleIndexed => format!(
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
