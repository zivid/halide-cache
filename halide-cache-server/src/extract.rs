//! Request extractors shared by the handlers.

use axum::{
    extract::{FromRequestParts, Path},
    http::{StatusCode, request::Parts},
};
use lager::Address;

/// The `{address}` path segment, parsed. Rejects with 400 when malformed.
pub struct BlobAddress(pub Address);

impl<S: Send + Sync> FromRequestParts<S> for BlobAddress {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StatusCode> {
        let Path(hex) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        Address::from_hex(&hex)
            .map(BlobAddress)
            .map_err(|_| StatusCode::BAD_REQUEST)
    }
}
