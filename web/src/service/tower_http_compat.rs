use std::{
    cell::RefCell,
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::stream::Stream;
use http_body::{Body, SizeHint};
use pin_project_lite::pin_project;
use xitca_http::{
    body::{none_body_hint, BodySize},
    util::service::router::{RouterGen, RouterMapErr},
};
use xitca_unsafe_collection::fake_send_sync::{FakeSend, FakeSync};

use crate::{
    bytes::Buf,
    context::WebContext,
    dev::service::{ready::ReadyService, Service},
    http::{header::HeaderMap, Request, RequestExt, Response, WebResponse},
};

/// A middleware type that bridge `xitca-service` and `tower-service`.
/// Any `tower-http` type that impl [tower::Service] trait can be passed to it and used as xitca-web's service type.
pub struct TowerHttpCompat<S> {
    service: S,
}

impl<S> TowerHttpCompat<S> {
    pub const fn new(service: S) -> Self
    where
        S: Clone,
    {
        Self { service }
    }
}

impl<S> Service for TowerHttpCompat<S>
where
    S: Clone,
{
    type Response = TowerCompatService<S>;
    type Error = Infallible;

    async fn call(&self, _: ()) -> Result<Self::Response, Self::Error> {
        let service = self.service.clone();

        Ok(TowerCompatService {
            service: RefCell::new(service),
        })
    }
}

impl<S> RouterGen for TowerHttpCompat<S> {
    type ErrGen<R> = RouterMapErr<R>;

    fn err_gen<R>(route: R) -> Self::ErrGen<R> {
        RouterMapErr(route)
    }
}

pub struct TowerCompatService<S> {
    service: RefCell<S>,
}

impl<S> TowerCompatService<S> {
    pub fn new(service: S) -> Self {
        Self {
            service: RefCell::new(service),
        }
    }
}

impl<'r, C, ReqB, S, ResB> Service<WebContext<'r, C, ReqB>> for TowerCompatService<S>
where
    S: tower_service::Service<Request<CompatBody<FakeSend<RequestExt<ReqB>>>>, Response = Response<ResB>>,
    ResB: Body,
    C: Clone + 'static,
    ReqB: Default,
{
    type Response = WebResponse<CompatBody<ResB>>;
    type Error = S::Error;

    async fn call(&self, mut ctx: WebContext<'r, C, ReqB>) -> Result<Self::Response, Self::Error> {
        let state = ctx.state().clone();
        let (mut parts, ext) = ctx.take_request().into_parts();
        parts.extensions.insert(FakeSync::new(FakeSend::new(state)));
        let req = Request::from_parts(parts, CompatBody::new(FakeSend::new(ext)));
        let fut = tower_service::Service::call(&mut *self.service.borrow_mut(), req);
        fut.await.map(|res| res.map(CompatBody::new))
    }
}

impl<S> ReadyService for TowerCompatService<S> {
    type Ready = ();

    #[inline]
    async fn ready(&self) -> Self::Ready {}
}

pin_project! {
    pub struct CompatBody<B> {
        #[pin]
        body: B
    }
}

impl<B> CompatBody<B> {
    pub fn new(body: B) -> Self {
        Self { body }
    }

    pub fn into_inner(self) -> B {
        self.body
    }
}

impl<B, T, E> Body for CompatBody<B>
where
    B: Stream<Item = Result<T, E>>,
    T: Buf,
{
    type Data = T;
    type Error = E;

    #[inline]
    fn poll_data(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        self.project().body.poll_next(cx)
    }

    #[inline]
    fn poll_trailers(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        Poll::Ready(Ok(None))
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        match BodySize::from_stream(&self.body) {
            BodySize::None => {
                let (low, upper) = none_body_hint();
                hint.set_lower(low as u64);
                hint.set_upper(upper.unwrap() as u64);
            }
            BodySize::Sized(size) => hint.set_exact(size as u64),
            BodySize::Stream => {}
        }

        hint
    }
}

impl<B> Stream for CompatBody<B>
where
    B: Body,
{
    type Item = Result<B::Data, B::Error>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().body.poll_data(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let hint = self.body.size_hint();
        (hint.lower() as usize, hint.upper().map(|num| num as usize))
    }
}

#[cfg(test)]
mod test {
    use xitca_http::body::{exact_body_hint, Once};

    use crate::bytes::Bytes;

    use super::*;

    #[test]
    fn body_compat() {
        let buf = Bytes::from_static(b"996");
        let len = buf.len();
        let body = CompatBody::new(Once::new(buf));

        let size = Body::size_hint(&body);

        assert_eq!(
            (size.lower() as usize, size.upper().map(|num| num as usize)),
            exact_body_hint(len)
        );

        let body = CompatBody::new(body);

        let size = Stream::size_hint(&body);

        assert_eq!(size, exact_body_hint(len));
    }
}
