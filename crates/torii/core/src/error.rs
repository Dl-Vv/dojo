use starknet::core::types::{FromByteSliceError, FromStrError};
use starknet::core::utils::CairoShortStringToFeltError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parsing error: {0}")]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("unsupported query clause")]
    UnsupportedQuery,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    FromStr(#[from] FromStrError),
    #[error(transparent)]
    CairoShortStringToFelt(#[from] CairoShortStringToFeltError),
    #[error(transparent)]
    FromByteSliceError(#[from] FromByteSliceError),
}
