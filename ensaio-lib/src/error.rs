//! Errors produced while loading or interpreting Ensaio specs.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum EnsaioError {
    #[error("spec load error: {0}")]
    Load(String),

    #[error("spec compile error: {0}")]
    Compile(String),

    #[error("interpreter error at {phase}: {message}")]
    Interp { phase: String, message: String },

    #[error("unknown assertion kind: {0}")]
    UnknownAssertionKind(String),
}

impl From<tatara_lisp::LispError> for EnsaioError {
    fn from(err: tatara_lisp::LispError) -> Self {
        EnsaioError::Compile(err.to_string())
    }
}
