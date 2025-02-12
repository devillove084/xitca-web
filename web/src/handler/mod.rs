//! type based high level async function service.

mod error;
mod impls;
mod types;

// TODO: enable sync handler when major wasm targets(including wasi) support std threads
#[cfg(not(target_family = "wasm"))]
mod sync;

pub use error::ExtractError;
pub use types::*;

#[cfg(not(target_family = "wasm"))]
pub use sync::handler_sync_service;

pub use xitca_http::util::service::handler::{handler_service, FromRequest, Responder};
