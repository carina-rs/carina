use std::io;

#[derive(Debug, thiserror::Error)]
enum PositiveCases {
    #[error("POS_DUP_EXPLICIT_NAMED: {error}")]
    ExplicitNamed {
        #[source]
        error: io::Error,
    },

    #[error("POS_INFO_LOSS")]
    InformationLoss {
        #[source]
        error: io::Error,
    },

    #[error("POS_TUPLE_INDEX_ONE: {1}")]
    TupleIndexOne(String, #[source] io::Error),

    #[error("POS_TUPLE_DOT_ONE: {}", .1)]
    TupleDotOne(String, #[source] io::Error),

    #[error("POS_SAME_LINE_NAMED: {source}")]
    SameLineNamed {
        #[source]
        source: io::Error,
    },

    #[error("POS_IMPLICIT_SOURCE")]
    ImplicitSource { source: io::Error },

    #[error("POS_POSITIONAL_FIELD: {}", .error)]
    PositionalField {
        #[source]
        error: io::Error,
    },

    #[error("POS_TRAILING_COMMENT: {source}")] // scanner must accept this comment
    TrailingComment {
        #[source]
        source: io::Error,
    },

    #[error("POS_FROM_INFO_LOSS")]
    FromInformationLoss(#[from] io::Error),
}

#[derive(thiserror::Error, Debug)]
#[error("POS_CALLER_RENDERED_DUP: {cause}")] // error-chain: caller-renders-sources
struct CallerRenderedDuplicate {
    #[source]
    cause: std::io::Error,
}
