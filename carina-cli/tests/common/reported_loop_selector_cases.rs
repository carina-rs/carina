//! Resource attributes whose rejected selector stops expression validation.

use std::path::Path;

pub struct Case {
    pub id: &'static str,
    pub declaration: &'static str,
    pub attributes: &'static str,
    pub errors: &'static [&'static str],
}

pub fn cases() -> Vec<Case> {
    const INDEXED: &str =
        "let children = for (_, item) in ['a'] { mock.test.resource { name = item } }\n";
    const INDEX_ERROR: &str = "cannot apply access .a to list(mock.test.resource)";
    vec![
        Case {
            id: "msib6",
            declaration: INDEXED,
            attributes: "name = \"${children.a.name}-${r.nope}\"",
            errors: &[INDEX_ERROR],
        },
        Case {
            id: "msib7-dot",
            declaration: INDEXED,
            attributes: "name = 'y' tags = { a = children.a.name, b = r.nope }",
            errors: &[INDEX_ERROR],
        },
        Case {
            id: "modcoal",
            declaration: INDEXED,
            attributes: "name = children.a.name ?? 'p'",
            errors: &[INDEX_ERROR],
        },
        Case {
            id: "modinterp",
            declaration: INDEXED,
            attributes: "name = \"${children.a.name}-x\"",
            errors: &[INDEX_ERROR],
        },
        Case {
            id: "modkey0",
            declaration: "let children = for k, v in ({ a = 'x' }) { mock.test.resource { name = v } }\n",
            attributes: "name = \"${children.0.name}-x\"",
            errors: &["cannot apply access .0 to keyed loop group"],
        },
        Case {
            id: "modidx",
            declaration: INDEXED,
            attributes: "name = children.a.name",
            errors: &[INDEX_ERROR],
        },
    ]
}

/// The map's first failed child has a different marker from its rejected selector.
pub fn input_i() -> Case {
    Case {
        id: "input-i",
        declaration: "let children = for (_, item) in ['a'] { mock.test.resource { name = item } }\n",
        attributes: "name = 'y' tags = { a = children[0].nope.x, b = children.a.name, c = r.nope }",
        errors: &["cannot apply access .a to list(mock.test.resource)"],
    }
}

/// Keep declarations and readers in separate files in both directory contexts.
pub fn fixture(root: &Path, case: &Case, module: bool) -> &'static str {
    let write = |name: &str, source: &str| {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    };
    write("providers.crn", "provider mock {}\n");
    write(
        "backend.crn",
        "backend local { path = 'carina.state.json' }\n",
    );
    let prefix = if module { "internal/" } else { "" };
    write(&format!("{prefix}loops.crn"), case.declaration);
    write(
        &format!("{prefix}references.crn"),
        "let r = mock.test.resource { name = 'x' }\n",
    );
    write(
        &format!("{prefix}reader.crn"),
        &format!("let o = mock.test.resource {{ {} }}\n", case.attributes),
    );
    if module {
        write("internal/arguments.crn", "arguments { value: String }\n");
        write(
            "main.crn",
            "let m = use { source = './internal' }\nlet instance = m { value = 'q' }\n",
        );
        "internal/reader.crn"
    } else {
        "reader.crn"
    }
}
