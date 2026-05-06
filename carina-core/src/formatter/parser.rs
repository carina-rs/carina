//! Pest parser for the formatter grammar

use pest::Parser;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "formatter/carina_fmt.pest"]
pub struct CarinaFmtParser;

/// Error type for format parsing
#[derive(Debug)]
pub struct FormatParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for FormatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Parse error at {}:{}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for FormatParseError {}

impl From<pest::error::Error<Rule>> for FormatParseError {
    fn from(err: pest::error::Error<Rule>) -> Self {
        let (line, column) = match err.line_col {
            pest::error::LineColLocation::Pos((l, c)) => (l, c),
            pest::error::LineColLocation::Span((l, c), _) => (l, c),
        };
        FormatParseError {
            message: err.variant.message().to_string(),
            line,
            column,
        }
    }
}

/// Parse source code for formatting
pub fn parse(source: &str) -> Result<pest::iterators::Pairs<'_, Rule>, FormatParseError> {
    CarinaFmtParser::parse(Rule::file, source).map_err(FormatParseError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_provider() {
        let input = "provider aws {\n    region = aws.Region.ap_northeast_1\n}\n";
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_with_comment() {
        let input = "# Header comment\nprovider aws {}\n";
        let result = parse(input);
        assert!(result.is_ok());
    }

    // Issue #904: The formatter grammar (carina_fmt.pest) is missing several
    // constructs that the main parser grammar (carina.pest) supports.
    // This causes `carina fmt --recursive` to fail on ~38 out of ~100 .crn files.

    #[test]
    fn issue_904_nested_block_in_resource_body() {
        // The formatter grammar's block_content only allows trivia | attribute,
        // but the main grammar also allows nested_block (identifier ~ "{" ~ ... ~ "}").
        // This affects files that use nested block syntax for List<Struct> fields
        // like security_group_ingress, lifecycle, etc.
        let input = r#"awscc.ec2.SecurityGroup {
  vpc_id = "vpc-123"

  security_group_ingress {
    ip_protocol = "tcp"
    from_port   = 80
    to_port     = 80
  }
}
"#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Nested block in resource body should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_904_nested_block_in_map_value() {
        // The formatter grammar's map_content only allows trivia | map_entry,
        // but the main grammar also allows nested_block inside maps.
        // This affects files like IAM role policies where `statement { ... }`
        // appears inside `assume_role_policy_document = { ... }`.
        let input = r#"awscc.iam.role {
  role_name_prefix = "test-"

  assume_role_policy_document = {
    version = "2012-10-17"
    statement {
      effect = "Allow"
      action = "sts:AssumeRole"
    }
  }
}
"#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Nested block inside map value should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_904_lifecycle_block_in_anonymous_resource() {
        // lifecycle { ... } is a nested block inside a resource body.
        // Without nested_block support in block_content, this fails.
        let input = r#"awscc.s3.Bucket {
  bucket_name_prefix = "test-"

  lifecycle {
    force_delete = true
  }
}
"#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Lifecycle block in anonymous resource should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_904_namespaced_id_with_digit_segment() {
        // The formatter grammar's namespaced_id only allows identifier segments,
        // but the main grammar allows digit-only segments (ASCII_DIGIT+).
        // This affects VPN Gateway resources that use `Type.ipsec.1`.
        let input = r#"awscc.ec2.vpn_gateway {
  type = awscc.ec2.vpn_gateway.Type.ipsec.1
}
"#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Namespaced ID with digit segment (ipsec.1) should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_904_multiple_nested_blocks_same_name() {
        // Multiple nested blocks with the same name (e.g., repeated ingress rules)
        // should be parsed as a list of blocks.
        let input = r#"awscc.ec2.SecurityGroup {
  group_description = "test"

  security_group_ingress {
    ip_protocol = "tcp"
    from_port   = 80
    to_port     = 80
  }

  security_group_ingress {
    ip_protocol = "tcp"
    from_port   = 443
    to_port     = 443
  }
}
"#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Multiple nested blocks with same name should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_index_access_syntax() {
        // Index access like a[0].b or a["key"].b should parse
        let input = r#"let x = items[0].name
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "Index access syntax should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_string_index_access() {
        let input = r#"let x = config["key"].value
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "String index access should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_for_expression() {
        // for expressions should parse
        let input = r#"let subnets = for subnet in subnets {
  awscc.ec2.Subnet {
    vpc_id = vpc.vpc_id
    cidr_block = subnet.cidr
  }
}
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "For expression should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_for_indexed_binding() {
        let input = r#"let items = for (i, x) in list {
  awscc.ec2.Subnet {
    name = x
  }
}
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "For expression with indexed binding should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_for_map_binding() {
        let input = r#"let items = for k, v in tags {
  awscc.ec2.Subnet {
    name = k
  }
}
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "For expression with map binding should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_read_resource_expr() {
        let input = r#"let vpc = read awscc.ec2.Vpc {
  vpc_id = "vpc-123"
}
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "Read resource expression should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_function_call_in_primary() {
        // function_call should be usable in primary position (not just in pipe)
        let input = r#"let x = concat(a, b)
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "Function call in primary position should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_1175_chained_field_access() {
        // Multiple chained field accesses: a.b.c
        let input = r#"let x = vpc.details.id
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "Chained field access should parse, got: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn issue_2504_let_binding_with_module_call_rhs() {
        // `let X = module_call { ... }` is accepted by the main parser
        // but rejected by the formatter parser. See carina-rs/carina#2504.
        let input = r#"let github = use {
  source = '../../../modules/github-oidc'
}

let github_actions_carina = github {
  github_repo         = 'carina-rs/infra'
  role_name           = 'github-actions-carina'
  managed_policy_arns = ['arn:aws:iam::aws:policy/AdministratorAccess']
}
"#;
        let result = parse(input);
        assert!(
            result.is_ok(),
            "let binding with module call RHS should parse, got: {}",
            result.unwrap_err()
        );
    }
}
