//! Build CST from pest parse tree

use pest::iterators::Pair;

use super::cst::{Cst, CstChild, CstNode, NodeKind, Span, Token, Trivia};
use super::parser::Rule;

/// Build a CST from pest parse result
pub fn build_cst(source: &str, pairs: pest::iterators::Pairs<'_, Rule>) -> Cst {
    let builder = CstBuilder::new(source);
    builder.build(pairs)
}

struct CstBuilder<'a> {
    source: &'a str,
}

impl<'a> CstBuilder<'a> {
    fn new(source: &'a str) -> Self {
        Self { source }
    }

    fn build(self, pairs: pest::iterators::Pairs<'a, Rule>) -> Cst {
        let mut children = Vec::new();

        for pair in pairs {
            if pair.as_rule() == Rule::file {
                for inner in pair.into_inner() {
                    if let Some(child) = self.build_child(inner) {
                        children.push(child);
                    }
                }
            }
        }

        let root =
            CstNode::with_children(NodeKind::File, Span::new(0, self.source.len()), children);

        Cst::new(root, self.source.to_string())
    }

    fn build_child(&self, pair: Pair<'a, Rule>) -> Option<CstChild> {
        let span = self.pair_span(&pair);

        match pair.as_rule() {
            Rule::EOI => None,

            // Trivia
            Rule::trivia => {
                let inner = pair.into_inner().next()?;
                self.build_child(inner)
            }
            Rule::ws => Some(CstChild::Trivia(Trivia::Whitespace(
                pair.as_str().to_string(),
            ))),
            Rule::newline => Some(CstChild::Trivia(Trivia::Newline)),
            Rule::comment => Some(CstChild::Trivia(Trivia::LineComment(
                pair.as_str().to_string(),
            ))),

            // Statements
            Rule::statement => {
                let inner = pair.into_inner().next()?;
                self.build_child(inner)
            }
            Rule::import_stmt => Some(CstChild::Node(self.build_node(NodeKind::ImportStmt, pair))),
            Rule::backend_block => Some(CstChild::Node(
                self.build_node(NodeKind::BackendBlock, pair),
            )),
            Rule::provider_block => Some(CstChild::Node(
                self.build_node(NodeKind::ProviderBlock, pair),
            )),
            Rule::input_block => Some(CstChild::Node(self.build_node(NodeKind::InputBlock, pair))),
            Rule::output_block => {
                Some(CstChild::Node(self.build_node(NodeKind::OutputBlock, pair)))
            }
            Rule::input_param => Some(CstChild::Node(self.build_node(NodeKind::InputParam, pair))),
            Rule::output_param => {
                Some(CstChild::Node(self.build_node(NodeKind::OutputParam, pair)))
            }
            Rule::type_expr => Some(CstChild::Node(self.build_node(NodeKind::TypeExpr, pair))),
            Rule::type_primitive => {
                Some(CstChild::Token(Token::new(pair.as_str().to_string(), span)))
            }
            Rule::type_list => Some(CstChild::Node(self.build_node(NodeKind::TypeExpr, pair))),
            Rule::type_map => Some(CstChild::Node(self.build_node(NodeKind::TypeExpr, pair))),
            Rule::type_ref => Some(CstChild::Node(self.build_node(NodeKind::TypeExpr, pair))),
            Rule::let_binding => Some(CstChild::Node(self.build_node(NodeKind::LetBinding, pair))),
            Rule::module_call => Some(CstChild::Node(self.build_node(NodeKind::ModuleCall, pair))),
            Rule::anonymous_resource => Some(CstChild::Node(
                self.build_node(NodeKind::AnonymousResource, pair),
            )),
            Rule::resource_expr => Some(CstChild::Node(
                self.build_node(NodeKind::ResourceExpr, pair),
            )),
            Rule::attribute => Some(CstChild::Node(self.build_node(NodeKind::Attribute, pair))),

            // Expressions
            Rule::expression => {
                let inner = pair.into_inner().next()?;
                self.build_child(inner)
            }
            Rule::pipe_expr => Some(CstChild::Node(self.build_node(NodeKind::PipeExpr, pair))),
            Rule::function_call => Some(CstChild::Node(
                self.build_node(NodeKind::FunctionCall, pair),
            )),
            Rule::primary => {
                let inner = pair.into_inner().next()?;
                self.build_child(inner)
            }
            Rule::env_var => Some(CstChild::Node(self.build_node(NodeKind::EnvVar, pair))),
            Rule::list => Some(CstChild::Node(self.build_node(NodeKind::List, pair))),
            Rule::map => Some(CstChild::Node(self.build_node(NodeKind::Map, pair))),
            Rule::map_content => None, // Silent rule
            Rule::map_entry => Some(CstChild::Node(self.build_node(NodeKind::MapEntry, pair))),
            Rule::variable_ref => {
                Some(CstChild::Node(self.build_node(NodeKind::VariableRef, pair)))
            }

            // Atoms - these become tokens
            Rule::namespaced_id => {
                Some(CstChild::Token(Token::new(pair.as_str().to_string(), span)))
            }
            Rule::identifier => Some(CstChild::Token(Token::new(pair.as_str().to_string(), span))),
            Rule::string => Some(CstChild::Token(Token::new(pair.as_str().to_string(), span))),
            Rule::float => Some(CstChild::Token(Token::new(pair.as_str().to_string(), span))),
            Rule::number => Some(CstChild::Token(Token::new(pair.as_str().to_string(), span))),
            Rule::boolean => Some(CstChild::Token(Token::new(pair.as_str().to_string(), span))),
            Rule::inner_string | Rule::char => None,

            // Delimiters and operators
            Rule::open_brace => Some(CstChild::Token(Token::new("{".to_string(), span))),
            Rule::close_brace => Some(CstChild::Token(Token::new("}".to_string(), span))),
            Rule::open_bracket => Some(CstChild::Token(Token::new("[".to_string(), span))),
            Rule::close_bracket => Some(CstChild::Token(Token::new("]".to_string(), span))),
            Rule::open_paren => Some(CstChild::Token(Token::new("(".to_string(), span))),
            Rule::close_paren => Some(CstChild::Token(Token::new(")".to_string(), span))),
            Rule::equals => Some(CstChild::Token(Token::new("=".to_string(), span))),
            Rule::colon => Some(CstChild::Token(Token::new(":".to_string(), span))),
            Rule::comma => Some(CstChild::Token(Token::new(",".to_string(), span))),
            Rule::pipe_op => Some(CstChild::Token(Token::new("|>".to_string(), span))),

            // Keywords
            Rule::kw_import => Some(CstChild::Token(Token::new("import".to_string(), span))),
            Rule::kw_as => Some(CstChild::Token(Token::new("as".to_string(), span))),
            Rule::kw_backend => Some(CstChild::Token(Token::new("backend".to_string(), span))),
            Rule::kw_provider => Some(CstChild::Token(Token::new("provider".to_string(), span))),
            Rule::kw_let => Some(CstChild::Token(Token::new("let".to_string(), span))),
            Rule::kw_env => Some(CstChild::Token(Token::new("env".to_string(), span))),
            Rule::kw_input => Some(CstChild::Token(Token::new("input".to_string(), span))),
            Rule::kw_output => Some(CstChild::Token(Token::new("output".to_string(), span))),
            Rule::kw_ref => Some(CstChild::Token(Token::new("ref".to_string(), span))),
            Rule::kw_list => Some(CstChild::Token(Token::new("list".to_string(), span))),
            Rule::kw_map => Some(CstChild::Token(Token::new("map".to_string(), span))),

            // Skip file_content (it's a silent rule wrapper)
            Rule::file_content => None,
            Rule::block_content => None,
            Rule::input_block_content => None,
            Rule::output_block_content => None,

            Rule::file => None,
        }
    }

    fn build_node(&self, kind: NodeKind, pair: Pair<'a, Rule>) -> CstNode {
        let span = self.pair_span(&pair);
        let mut children = Vec::new();

        for inner in pair.into_inner() {
            if let Some(child) = self.build_child(inner) {
                children.push(child);
            }
        }

        CstNode::with_children(kind, span, children)
    }

    fn pair_span(&self, pair: &Pair<'a, Rule>) -> Span {
        let pest_span = pair.as_span();
        Span::new(pest_span.start(), pest_span.end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatter::parser;

    #[test]
    fn test_build_simple_cst() {
        let input = "provider aws {}\n";
        let pairs = parser::parse(input).unwrap();
        let cst = build_cst(input, pairs);

        assert_eq!(cst.root.kind, NodeKind::File);
        assert!(!cst.root.children.is_empty());
    }

    #[test]
    fn test_build_cst_with_comment() {
        let input = "# Comment\nprovider aws {}\n";
        let pairs = parser::parse(input).unwrap();
        let cst = build_cst(input, pairs);

        // First child should be the comment
        let first_child = &cst.root.children[0];
        match first_child {
            CstChild::Trivia(Trivia::LineComment(s)) => {
                assert_eq!(s, "# Comment");
            }
            _ => panic!("Expected comment trivia"),
        }
    }
}
