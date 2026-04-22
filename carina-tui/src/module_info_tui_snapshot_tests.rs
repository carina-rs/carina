//! TUI snapshot tests for the module info viewer.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use carina_core::module::FileSignature;
use carina_core::parser::{ProviderContext, parse};

use crate::module_info_app::ModuleInfoApp;
use crate::module_info_ui::draw;
use crate::test_utils::buffer_to_string;

/// Render the module info TUI into a string.
fn render_module_info(signature: &FileSignature, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = ModuleInfoApp::new(signature);

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

/// Render with selection at a specific row.
fn render_module_info_selected(
    signature: &FileSignature,
    width: u16,
    height: u16,
    selection: usize,
) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = ModuleInfoApp::new(signature);

    for _ in 0..selection {
        app.move_down();
    }

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

fn build_module_signature() -> FileSignature {
    let input = r#"
        arguments {
            vpc: aws.vpc {
                description = "The VPC to deploy into"
            }
            enable_https: Bool = true
        }

        attributes {
            security_group: aws.security_group = web_sg.id
        }

        let web_sg = aws.security_group {
            name   = "web-sg"
            vpc_id = vpc
        }

        let http_rule = aws.security_group.ingress_rule {
            name              = "http"
            security_group_id = web_sg.id
            from_port         = 80
            to_port           = 80
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    FileSignature::from_parsed_file_with_name(&parsed, "web_tier")
}

fn build_root_config_signature() -> FileSignature {
    let input = r#"
        provider aws {
            region = aws.Region.ap_northeast_1
        }

        let main_vpc = aws.vpc {
            name       = "main-vpc"
            cidr_block = "10.0.0.0/16"
        }

        let subnet = aws.subnet {
            name       = "main-subnet"
            vpc_id     = main_vpc.vpc_id
            cidr_block = "10.0.1.0/24"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    FileSignature::from_parsed_file_with_name(&parsed, "main")
}

#[test]
fn snapshot_module_info_default() {
    let sig = build_module_signature();
    let output = render_module_info(&sig, 100, 30);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_module_info_selected_argument() {
    let sig = build_module_signature();
    // Select first argument (row 1, after ARGUMENTS header)
    let output = render_module_info_selected(&sig, 100, 30, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_module_info_selected_resource() {
    let sig = build_module_signature();
    // Select first resource (after ARGUMENTS header + 2 args + CREATES header = row 4)
    let output = render_module_info_selected(&sig, 100, 30, 4);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_root_config_info() {
    let sig = build_root_config_signature();
    let output = render_module_info(&sig, 100, 30);
    insta::assert_snapshot!(output);
}
