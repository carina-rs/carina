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
        let input = r#"awscc.ec2.security_group {
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
        let input = r#"awscc.s3.bucket {
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
        let input = r#"awscc.ec2.security_group {
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
}
