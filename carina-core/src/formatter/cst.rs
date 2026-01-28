//! Concrete Syntax Tree for formatting
//! Preserves all source information including trivia (whitespace and comments)

// Allow dead code for fields/methods that may be used in future enhancements
#![allow(dead_code)]

/// Source location information
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Trivia types (whitespace and comments)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trivia {
    /// Spaces and tabs (not including newlines)
    Whitespace(String),
    /// Single newline
    Newline,
    /// Line comment including the # prefix
    LineComment(String),
}

impl Trivia {
    pub fn text(&self) -> &str {
        match self {
            Trivia::Whitespace(s) => s,
            Trivia::Newline => "\n",
            Trivia::LineComment(s) => s,
        }
    }
}

/// A token with its text and position
#[derive(Debug, Clone)]
pub struct Token {
    pub text: String,
    pub span: Span,
}

impl Token {
    pub fn new(text: String, span: Span) -> Self {
        Self { text, span }
    }
}

/// CST node kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    ImportStmt,
    BackendBlock,
    ProviderBlock,
    LetBinding,
    ModuleCall,
    AnonymousResource,
    ResourceExpr,
    Attribute,
    Expression,
    PipeExpr,
    FunctionCall,
    EnvVar,
    VariableRef,
    NamespacedId,
    Identifier,
    String,
    Number,
    Boolean,
    List,
    // Delimiters
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    OpenParen,
    CloseParen,
    Equals,
    PipeOperator,
    Comma,
    // Keywords
    KwImport,
    KwAs,
    KwBackend,
    KwProvider,
    KwLet,
    KwEnv,
}

/// CST node - preserves all source information
#[derive(Debug, Clone)]
pub struct CstNode {
    pub kind: NodeKind,
    pub span: Span,
    pub children: Vec<CstChild>,
}

impl CstNode {
    pub fn new(kind: NodeKind, span: Span) -> Self {
        Self {
            kind,
            span,
            children: Vec::new(),
        }
    }

    pub fn with_children(kind: NodeKind, span: Span, children: Vec<CstChild>) -> Self {
        Self {
            kind,
            span,
            children,
        }
    }
}

/// Child of a CST node - can be a node, token, or trivia
#[derive(Debug, Clone)]
pub enum CstChild {
    Node(CstNode),
    Token(Token),
    Trivia(Trivia),
}

/// Complete CST for a file
#[derive(Debug)]
pub struct Cst {
    pub root: CstNode,
    pub source: String,
}

impl Cst {
    pub fn new(root: CstNode, source: String) -> Self {
        Self { root, source }
    }

    /// Get the source text for a span
    pub fn text(&self, span: &Span) -> &str {
        &self.source[span.start..span.end]
    }
}
