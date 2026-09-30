use crate::metrics::Client;
use axum::{
    extract::{ConnectInfo, FromRequestParts, Path},
    http::{StatusCode, request::Parts},
};
use lager::Address;
use std::net::{IpAddr, SocketAddr};

const CLIENT_HEADER: &str = "x-halide-cache-client";

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

pub struct ClientId(pub Client);

impl<S: Send + Sync> FromRequestParts<S> for ClientId {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StatusCode> {
        let ConnectInfo(peer) = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let ip: IpAddr = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(peer.ip());
        let hostname = parts
            .headers
            .get(CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().chars().take(128).collect::<String>())
            .filter(|v| !v.is_empty());
        Ok(ClientId(Client { ip, hostname }))
    }
}
