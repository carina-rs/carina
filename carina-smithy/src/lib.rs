pub mod ast;
pub mod query;

pub use ast::*;
pub use query::ShapeKind;

/// Parse a Smithy 2.0 JSON AST model from a JSON string.
pub fn parse(json: &str) -> Result<SmithyModel, serde_json::Error> {
    serde_json::from_str(json)
}

/// Parse a Smithy 2.0 JSON AST model from a reader.
pub fn parse_reader<R: std::io::Read>(reader: R) -> Result<SmithyModel, serde_json::Error> {
    serde_json::from_reader(reader)
}
