use std::convert::Infallible;

use xitca_service::Service;

use super::service::H3Service;

/// Http/3 Builder type.
/// Take in generic types of ServiceFactory for `quinn`.
pub struct H3ServiceBuilder;

impl Default for H3ServiceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl H3ServiceBuilder {
    /// Construct a new Service Builder with given service factory.
    pub fn new() -> Self {
        H3ServiceBuilder
    }
}

impl<S> Service<S> for H3ServiceBuilder {
    type Response = H3Service<S>;
    type Error = Infallible;

    async fn call(&self, service: S) -> Result<Self::Response, Self::Error> {
        Ok(H3Service::new(service))
    }
}
