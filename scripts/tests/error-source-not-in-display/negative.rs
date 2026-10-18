use std::io;

#[derive(Debug, thiserror::Error)]
enum NegativeCases {
    #[error(transparent)]
    TransparentSource(#[source] io::Error),

    #[error(transparent)]
    TransparentFrom(#[from] io::Error),

    #[error("NEG_EMBEDDED_WITHOUT_SOURCE: {error}")]
    EmbeddedWithoutSource { error: io::Error },

    #[error("NEG_LEAF_TUPLE: {1}")]
    LeafTuple(String, io::Error),

    #[error("NEG_OK")]
    Ok,
}
