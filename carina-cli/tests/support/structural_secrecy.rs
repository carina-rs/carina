//! Shared CLI/LSP acceptance inputs and core-owned limit messages.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use carina_core::secrecy::LimitKind;

pub const CANARY: &str = "STRUCTURAL_SECRECY_4470_CANARY";
pub const COALESCE_MESSAGE: &str = "Attribute 'comment': ?? alternatives mix secret and public values; all alternatives must be secret or all public.";
pub const LOOKUP_MESSAGE: &str = "Attribute 'comment': lookup() alternatives mix secret and public values; all alternatives must be secret or all public.";
pub static DEPTH_LIMIT_MESSAGE: LazyLock<String> =
    LazyLock::new(|| LimitKind::Depth.message("Attribute", "comment"));
pub static VISITS_LIMIT_MESSAGE: LazyLock<String> =
    LazyLock::new(|| LimitKind::ReferenceVisits.message("Attribute", "comment"));
pub const LET_COALESCE_MESSAGE: &str = "Let binding 't': ?? alternatives mix secret and public values; all alternatives must be secret or all public.";
pub const ARGUMENT_COALESCE_MESSAGE: &str = "Module argument 'value': ?? alternatives mix secret and public values; all alternatives must be secret or all public.";
pub const EXPORT_COALESCE_MESSAGE: &str = "Export 'x': ?? alternatives mix secret and public values; all alternatives must be secret or all public.";

pub fn mixed_and_default_tag_fixture() -> tempfile::TempDir {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(
        fixture.path().join("providers.crn"),
        "provider mock { default_tags = { Owner = seed.identifier } }\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("backend.crn"), "backend local {}\n").unwrap();
    std::fs::write(fixture.path().join("main.crn"), format!("let seed = mock.test.resource {{ name = 'seed' }}\nlet sink = mock.test.resource {{ name = 'sink' comment = 'public' ?? secret('{CANARY}') }}\n")).unwrap();
    fixture
}

/// One-based Unicode scalar coordinates, matching CLI source metadata.
/// LSP converts these to zero-based UTF-16 only when rendering.
#[derive(Clone, Copy, Debug)]
pub struct Site {
    pub file: &'static str,
    pub line: u32,
    pub column: u32,
    pub end: Option<(u32, u32)>,
}

impl Site {
    pub const fn token(file: &'static str, line: u32, column: u32, width: u32) -> Self {
        Self {
            file,
            line,
            column,
            end: Some((line, column + width)),
        }
    }

    pub const fn attribute(line: u32) -> Self {
        Self {
            file: "main.crn",
            line,
            column: 3,
            end: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Finding {
    pub site: Site,
    pub message: &'static str,
}

impl Finding {
    const fn coalesce(line: u32, column: u32) -> Self {
        Self {
            site: Site::token("main.crn", line, column, 2),
            message: COALESCE_MESSAGE,
        }
    }

    const fn lookup(line: u32, column: u32, end_column: u32) -> Self {
        Self {
            site: Site::token("main.crn", line, column, end_column - column),
            message: LOOKUP_MESSAGE,
        }
    }

    pub fn depth_limit(line: u32) -> Self {
        Self {
            site: Site::attribute(line),
            message: DEPTH_LIMIT_MESSAGE.as_str(),
        }
    }

    pub fn visits_limit(line: u32) -> Self {
        Self {
            site: Site::attribute(line),
            message: VISITS_LIMIT_MESSAGE.as_str(),
        }
    }
}

// Pin the authored ?? token, including the inner operator of a discarded
// wrapped alternative. An outer secret() must not suppress the inner error.
pub const COALESCE_FINDINGS: &[Finding] = &[
    Finding::coalesce(3, 27),
    Finding::coalesce(7, 28),
    Finding::coalesce(11, 54),
    Finding::coalesce(15, 22),
    Finding::coalesce(19, 37),
    Finding::coalesce(23, 51),
    Finding::coalesce(27, 17),
];

// lookup findings cover the authored call, excluding any enclosing secret().
pub const LOOKUP_FINDINGS: &[Finding] = &[
    Finding::lookup(3, 13, 94),
    Finding::lookup(7, 13, 89),
    Finding::lookup(11, 13, 96),
    Finding::lookup(15, 13, 70),
    Finding::lookup(19, 13, 68),
    Finding::lookup(23, 13, 82),
    Finding::lookup(27, 20, 96),
];

pub const MODULE_FINDING: Finding = Finding {
    site: Site::token("module/main.crn", 3, 19, 2),
    message: COALESCE_MESSAGE,
};

pub const MODULE_IF_FINDING: Finding = Finding {
    site: Site::token("module/main.crn", 4, 21, 2),
    message: COALESCE_MESSAGE,
};

pub const SECRET_CALL: Site = Site {
    file: "main.crn",
    line: 2,
    column: 23,
    end: Some((5, 2)),
};

fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

pub fn fixture(name: &str) -> tempfile::TempDir {
    // This module is also included by carina-lsp's integration tests.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = workspace
        .join("carina-cli/tests/fixtures/structural_secrecy")
        .join(name);
    let fixture = tempfile::tempdir().unwrap();
    copy_directory(&source, fixture.path());
    fixture
}

pub fn public_module_fixture() -> tempfile::TempDir {
    let fixture = fixture("module_instances");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let template = use { source = \"./module\" }\nlet public_instance = template {\n  name = \"public-instance\"\n  value = \"public\"\n}\n",
    )
    .unwrap();
    fixture
}

pub const ROOT_AND_MODULE_FINDINGS: &[Finding] = &[MODULE_FINDING, Finding::coalesce(13, 27)];

pub fn root_and_module_fixture() -> tempfile::TempDir {
    let fixture = fixture("module_instances");
    let path = fixture.path().join("main.crn");
    let mut root = std::fs::read_to_string(&path).unwrap();
    root.push_str("let other = mock.test.resource { name = 'other' comment = secret('canary') }\nlet sink = mock.test.resource {\n  name = 'sink'\n  comment = other.comment ?? \"default\"\n}\n");
    std::fs::write(path, root).unwrap();
    fixture
}

pub fn unexpandable_root_and_module_fixture() -> tempfile::TempDir {
    let fixture = root_and_module_fixture();
    std::fs::write(
        fixture.path().join("module/arguments.crn"),
        "arguments { name: String value: String required: String }\n",
    )
    .unwrap();
    fixture
}

pub fn root_and_module_with_backend_error_fixture() -> tempfile::TempDir {
    let fixture = root_and_module_fixture();
    std::fs::write(
        fixture.path().join("backend.crn"),
        "backend local { path = 'carina.state.json' typo = true }\n",
    )
    .unwrap();
    fixture
}

pub fn declaration_kinds_fixture() -> (tempfile::TempDir, Vec<Finding>) {
    let fixture = fixture("module_instances");
    let source = "let t = 'public' ?? secret('canary')\nexports { x: String = 'public' ?? secret('canary') }\nlet template = use { source = './module' }\nlet instance = template {\n  name = 'instance'\n  value = 'public' ?? secret('canary')\n}\n";
    std::fs::write(fixture.path().join("main.crn"), source).unwrap();
    std::fs::write(
        fixture.path().join("module/main.crn"),
        "mock.test.resource { name = name comment = value }\n",
    )
    .unwrap();
    let findings = [
        (1, LET_COALESCE_MESSAGE),
        (2, EXPORT_COALESCE_MESSAGE),
        (6, ARGUMENT_COALESCE_MESSAGE),
    ]
    .into_iter()
    .map(|(line, message)| Finding {
        site: Site::token(
            "main.crn",
            line,
            source
                .lines()
                .nth((line - 1) as usize)
                .unwrap()
                .find("??")
                .unwrap() as u32
                + 1,
            2,
        ),
        message,
    })
    .collect();
    (fixture, findings)
}

pub fn module_if_fixture() -> tempfile::TempDir {
    let fixture = fixture("module_instances");
    std::fs::write(
        fixture.path().join("module/arguments.crn"),
        "arguments {\n  name: String\n  value: String\n  enabled: Bool = true\n}\n",
    )
    .unwrap();
    // ModuleIf selects a resource only after argument substitution. Preserve
    // the operator's authored location through that selection as well.
    std::fs::write(
        fixture.path().join("module/main.crn"),
        "let item = if enabled {\n  mock.test.resource {\n    name = name\n    comment = value ?? \"default\"\n  }\n} else {\n  mock.test.resource { name = name comment = \"public\" }\n}\n",
    )
    .unwrap();
    fixture
}

#[derive(Clone, Copy, Debug)]
pub enum ChainShape {
    SingleFile,
    SiblingFiles,
}

pub const CHAIN_SHAPES: [ChainShape; 2] = [ChainShape::SingleFile, ChainShape::SiblingFiles];

fn single_file_preamble(root: &Path) -> String {
    let mut source = String::new();
    for file in ["providers.crn", "backend.crn"] {
        let path = root.join(file);
        source.push_str(&std::fs::read_to_string(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }
    source
}

/// r128.comment follows 128 references; r127 is the adjacent accepted case.
/// Keep a shallow consumer in both orders to expose invalid memo reuse.
/// Deeper chains have one finding at every r128..r(last) attribute.
pub fn depth_fixture(
    last: usize,
    shallow_first: bool,
    shape: ChainShape,
) -> (tempfile::TempDir, Vec<Finding>) {
    depth_fixture_with_expression(last, shallow_first, shape, str::to_owned)
}

#[derive(Clone, Copy, Debug)]
pub enum ExpressionWrapper {
    Builtin,
    List,
}

pub const EXPRESSION_WRAPPERS: [ExpressionWrapper; 2] =
    [ExpressionWrapper::Builtin, ExpressionWrapper::List];

impl ExpressionWrapper {
    fn wrap(self, expression: &str) -> String {
        match self {
            Self::Builtin => format!("upper({expression})"),
            Self::List => format!("join(\"\", [{expression}])"),
        }
    }
}

pub fn expression_depth_fixture(
    last: usize,
    shallow_first: bool,
    shape: ChainShape,
    wrapper: ExpressionWrapper,
) -> (tempfile::TempDir, Vec<Finding>) {
    depth_fixture_with_expression(last, shallow_first, shape, |reference| {
        wrapper.wrap(reference)
    })
}

/// A single reference, with enough expression nesting to expose depth charging
/// while staying below the parser's separate syntax stack limits (#4481).
pub fn deep_expression_fixture(
    shape: ChainShape,
    wrapper: ExpressionWrapper,
    levels: usize,
) -> tempfile::TempDir {
    depth_fixture_with_expression(1, false, shape, |reference| {
        (0..levels).fold(reference.to_string(), |expression, _| {
            wrapper.wrap(&expression)
        })
    })
    .0
}

fn depth_fixture_with_expression(
    last: usize,
    shallow_first: bool,
    shape: ChainShape,
    expression: impl Fn(&str) -> String,
) -> (tempfile::TempDir, Vec<Finding>) {
    let fixture = fixture("limits");
    let path = fixture.path().join("resources.crn");
    let mut resources = std::fs::read_to_string(&path).unwrap();
    for index in 1..last {
        writeln!(
            resources,
            "let r{index} = mock.test.resource {{\n  name = \"r{index}\"\n  comment = {}\n}}",
            expression(&format!("r{}.comment", index - 1))
        )
        .unwrap();
    }
    let resource_lines = resources.lines().count() as u32;
    let shallow = "let shallow = mock.test.resource { name = \"shallow\" comment = r0.comment }\n";
    let deep = format!(
        "let r{last} = mock.test.resource {{\n  name = \"r{last}\"\n  comment = {}\n}}\n",
        expression(&format!("r{}.comment", last - 1))
    );
    let main = if shallow_first {
        format!("{shallow}{deep}")
    } else {
        format!("{deep}{shallow}")
    };
    let (resource_file, resource_offset, main_offset) = match shape {
        ChainShape::SingleFile => {
            let preamble = single_file_preamble(fixture.path());
            let offset = preamble.lines().count() as u32;
            std::fs::remove_file(path).unwrap();
            std::fs::write(
                fixture.path().join("main.crn"),
                preamble + &resources + &main,
            )
            .unwrap();
            ("main.crn", offset, offset + resource_lines)
        }
        ChainShape::SiblingFiles => {
            std::fs::write(path, resources).unwrap();
            std::fs::write(fixture.path().join("main.crn"), main).unwrap();
            ("resources.crn", 0, 0)
        }
    };
    let mut findings = Vec::new();
    for index in 128..last {
        // r0 and every intermediate resource occupy four lines.
        let line = resource_offset + 4 * index as u32 + 3;
        findings.push(Finding {
            site: Site {
                file: resource_file,
                end: Some((
                    line,
                    format!(
                        "  comment = {}",
                        expression(&format!("r{}.comment", index - 1))
                    )
                    .len() as u32
                        + 1,
                )),
                ..Site::attribute(line)
            },
            message: DEPTH_LIMIT_MESSAGE.as_str(),
        });
    }
    if last >= 128 {
        let line = main_offset + if shallow_first { 4 } else { 3 };
        let mut finding = Finding::depth_limit(line);
        finding.site.end = Some((
            line,
            format!(
                "  comment = {}",
                expression(&format!("r{}.comment", last - 1))
            )
            .len() as u32
                + 1,
        ));
        findings.push(finding);
    }
    (fixture, findings)
}

/// Each forwarding instance adds an output reference and an argument reference.
/// The sink follows 126 references with 63 instances and 128 with 64 instances.
pub fn module_output_depth_fixture(
    instances: usize,
    shape: ChainShape,
) -> (tempfile::TempDir, Vec<Finding>) {
    assert!(matches!(instances, 63 | 64));
    let fixture = fixture("limits");
    let module = fixture.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let arguments = "arguments { value: String }\n";
    let output = "attributes { output: String = value }\n";
    match shape {
        ChainShape::SingleFile => {
            std::fs::write(module.join("main.crn"), arguments.to_string() + output).unwrap();
        }
        ChainShape::SiblingFiles => {
            std::fs::write(module.join("arguments.crn"), arguments).unwrap();
            std::fs::write(module.join("main.crn"), output).unwrap();
        }
    }
    let mut resources = "let template = use { source = './module' }\n".to_string();
    for index in 1..=instances {
        let value = if index == 1 {
            format!("secret(\"{CANARY}\")")
        } else {
            format!("m{}.output", index - 1)
        };
        writeln!(resources, "let m{index} = template {{ value = {value} }}").unwrap();
    }
    let consumer = format!(
        "let sink = mock.test.resource {{\n  name = \"sink\"\n  comment = m{instances}.output\n}}\n"
    );
    let (main, offset) = match shape {
        ChainShape::SingleFile => {
            let preamble = single_file_preamble(fixture.path());
            let offset = (preamble.lines().count() + resources.lines().count()) as u32;
            std::fs::remove_file(fixture.path().join("resources.crn")).unwrap();
            (preamble + &resources + &consumer, offset)
        }
        ChainShape::SiblingFiles => {
            std::fs::write(fixture.path().join("resources.crn"), resources).unwrap();
            (consumer, 0)
        }
    };
    std::fs::write(fixture.path().join("main.crn"), main).unwrap();
    let findings = if instances == 64 {
        let line = offset + 3;
        let mut finding = Finding::depth_limit(line);
        finding.site.end = Some((
            line,
            format!("  comment = m{instances}.output").len() as u32 + 1,
        ));
        vec![finding]
    } else {
        vec![]
    };
    (fixture, findings)
}

fn reference_list_resource(name: &str, visits: usize) -> String {
    let mut source = format!(
        "let {name} = mock.test.resource {{\n  name = \"{name}\"\n  comment = join(\"\", [\n"
    );
    for _ in 0..visits {
        source.push_str("    r0.comment,\n");
    }
    source.push_str("  ])\n}\n");
    source
}

/// The first consumer uses the entire budget; each attribute must start afresh.
/// Every list element references the same target, so cache hits still count.
pub fn visits_fixture(visits: usize, shape: ChainShape) -> (tempfile::TempDir, Finding) {
    let fixture = fixture("limits");
    let first = reference_list_resource("first", 4096);
    let mut second_line = first.lines().count() as u32 + 3;
    let main = first + &reference_list_resource("second", visits);
    let main = match shape {
        ChainShape::SingleFile => {
            let preamble = single_file_preamble(fixture.path());
            let path = fixture.path().join("resources.crn");
            let resources = std::fs::read_to_string(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            second_line += (preamble.lines().count() + resources.lines().count()) as u32;
            preamble + &resources + &main
        }
        ChainShape::SiblingFiles => main,
    };
    std::fs::write(fixture.path().join("main.crn"), main).unwrap();
    let mut finding = Finding::visits_limit(second_line);
    finding.site.end = Some((second_line + visits as u32 + 1, 5));
    (fixture, finding)
}

pub fn fan_out_fixture() -> tempfile::TempDir {
    let fixture = fixture("limits");
    let path = fixture.path().join("resources.crn");
    let mut resources = std::fs::read_to_string(&path).unwrap();
    for index in 1..=13 {
        writeln!(
            resources,
            "let r{index} = mock.test.resource {{\n  name = \"r{index}\"\n  comment = join(\"\", [r{previous}.comment, r{previous}.comment])\n}}",
            previous = index - 1
        )
        .unwrap();
    }
    std::fs::write(path, resources).unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "let first = mock.test.resource { name = \"first\" comment = r13.comment }\nlet second = mock.test.resource { name = \"second\" comment = r13.comment }\n",
    )
    .unwrap();
    fixture
}

pub fn is_secrecy_message(message: &str) -> bool {
    [
        COALESCE_MESSAGE,
        LOOKUP_MESSAGE,
        DEPTH_LIMIT_MESSAGE.as_str(),
        VISITS_LIMIT_MESSAGE.as_str(),
        LET_COALESCE_MESSAGE,
        ARGUMENT_COALESCE_MESSAGE,
        EXPORT_COALESCE_MESSAGE,
    ]
    .iter()
    .any(|expected| message.starts_with(expected))
}
