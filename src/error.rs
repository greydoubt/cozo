use std::result;
use thiserror::Error;
use crate::parser::Rule;

#[derive(Error, Debug)]
pub enum CozoError {
    #[error("Invalid UTF code")]
    InvalidUtfCode,

    #[error("Invalid escape sequence")]
    InvalidEscapeSequence,

    #[error("Type mismatch")]
    TypeError,

    #[error("Reserved identifier")]
    ReservedIdent,

    #[error("The requested name exists")]
    NameConflict,

    #[error("Undefined type")]
    UndefinedType,

    #[error("Wrong type")]
    WrongType,

    #[error("Cannot have global edge between local nodes")]
    IncompatibleEdge,

    #[error("Unexpected index columns found")]
    UnexpectedIndexColumns,

    #[error("Database already closed")]
    DatabaseClosed,

    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),

    #[error(transparent)]
    ParseFloat(#[from] std::num::ParseFloatError),

    #[error(transparent)]
    Parse(#[from] pest::error::Error<Rule>),

    // #[error(transparent)]
    // Storage(#[from] rocksdb::Error)
}

pub type Result<T> = result::Result<T, CozoError>;