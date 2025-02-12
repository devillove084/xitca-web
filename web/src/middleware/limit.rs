use std::{
    cell::RefCell,
    convert::Infallible,
    error, fmt,
    pin::Pin,
    task::{ready, Context, Poll},
};

use futures_core::stream::Stream;
use pin_project_lite::pin_project;
use xitca_http::Request;

use crate::{
    body::BodyStream,
    context::WebContext,
    dev::service::{pipeline::PipelineE, ready::ReadyService, Service},
    handler::Responder,
    http::{const_header_value::TEXT_UTF8, header::CONTENT_TYPE, status::StatusCode, WebResponse},
};

#[derive(Copy, Clone)]
pub struct Limit {
    request_body_size: usize,
}

impl Default for Limit {
    fn default() -> Self {
        Self::new()
    }
}

impl Limit {
    pub fn new() -> Self {
        Self {
            request_body_size: usize::MAX,
        }
    }

    /// Set max size in byte unit the request body can be.
    pub fn set_request_body_max_size(mut self, size: usize) -> Self {
        self.request_body_size = size;
        self
    }
}

impl<S> Service<S> for Limit {
    type Response = LimitService<S>;
    type Error = Infallible;

    async fn call(&self, service: S) -> Result<Self::Response, Self::Error> {
        Ok(LimitService { service, limit: *self })
    }
}

pub struct LimitService<S> {
    service: S,
    limit: Limit,
}

pub type LimitServiceError<E> = PipelineE<LimitError, E>;

impl<'r, S, C, B, Res, Err> Service<WebContext<'r, C, B>> for LimitService<S>
where
    B: BodyStream + Default,
    S: for<'r2> Service<WebContext<'r2, C, LimitBody<B>>, Response = Res, Error = Err>,
{
    type Response = Res;
    type Error = LimitServiceError<Err>;

    async fn call(&self, mut ctx: WebContext<'r, C, B>) -> Result<Self::Response, Self::Error> {
        let (parts, ext) = ctx.take_request().into_parts();
        let ctx = ctx.ctx;
        let (ext, body) = ext.replace_body(());
        let mut body = RefCell::new(LimitBody::new(body, self.limit.request_body_size));
        let mut req = Request::from_parts(parts, ext);

        let ctx = WebContext::new(&mut req, &mut body, ctx);

        self.service.call(ctx).await.map_err(LimitServiceError::Second)
    }
}

impl<S> ReadyService for LimitService<S>
where
    S: ReadyService,
{
    type Ready = S::Ready;

    #[inline]
    async fn ready(&self) -> Self::Ready {
        self.service.ready().await
    }
}

pin_project! {
    pub struct LimitBody<B> {
        limit: usize,
        record: usize,
        #[pin]
        body: B
    }
}

impl<B: Default> Default for LimitBody<B> {
    fn default() -> Self {
        Self {
            limit: 0,
            record: 0,
            body: B::default(),
        }
    }
}

impl<B> LimitBody<B> {
    fn new(body: B, limit: usize) -> Self {
        Self { limit, record: 0, body }
    }
}

impl<B> Stream for LimitBody<B>
where
    B: BodyStream,
{
    type Item = Result<B::Chunk, LimitBodyError<B::Error>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        if *this.record >= *this.limit {
            return Poll::Ready(Some(Err(LimitBodyError::First(LimitError::BodyOverSize(*this.limit)))));
        }

        match ready!(this.body.poll_next(cx)) {
            Some(res) => {
                let chunk = res.map_err(LimitBodyError::Second)?;
                *this.record += chunk.as_ref().len();
                // TODO: for now there is no way to split a chunk if it goes beyond body limit.
                Poll::Ready(Some(Ok(chunk)))
            }
            None => Poll::Ready(None),
        }
    }
}

pub type LimitBodyError<E> = PipelineE<LimitError, E>;

#[derive(Debug)]
pub enum LimitError {
    BodyOverSize(usize),
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BodyOverSize(size) => write!(f, "Body size reached limit: {size} bytes."),
        }
    }
}

impl error::Error for LimitError {}

impl<'r, C, B> Responder<WebContext<'r, C, B>> for LimitError {
    type Output = WebResponse;

    async fn respond_to(self, req: WebContext<'r, C, B>) -> Self::Output {
        let mut res = req.into_response(format!("{self}"));
        res.headers_mut().insert(CONTENT_TYPE, TEXT_UTF8);
        *res.status_mut() = StatusCode::BAD_REQUEST;
        res
    }
}

#[cfg(test)]
mod test {
    use core::{future::poll_fn, pin::pin};

    use xitca_unsafe_collection::futures::NowOrPanic;

    use crate::{
        body::BoxStream,
        bytes::Bytes,
        error::BodyError,
        handler::{body::Body, handler_service},
        http::{Request, RequestExt},
        test::collect_body,
        App,
    };

    use super::*;

    async fn handler<B: BodyStream>(Body(body): Body<B>) -> String {
        let mut body = pin!(body);

        let chunk = poll_fn(|cx| body.as_mut().poll_next(cx)).await.unwrap().unwrap();

        assert!(poll_fn(|cx| body.as_mut().poll_next(cx)).await.unwrap().is_err());

        std::str::from_utf8(chunk.as_ref()).unwrap().to_string()
    }

    #[test]
    fn request_body_over_limit() {
        use futures_util::stream::{self, StreamExt};

        let chunk = b"hello,world!";

        let item = || async { Ok::<_, BodyError>(Bytes::from_static(chunk)) };

        let body = stream::once(item()).chain(stream::once(item()));
        let ext = RequestExt::default().map_body(|_: ()| BoxStream::new(body));
        let req = Request::new(ext);

        let body = App::new()
            .at("/", handler_service(handler))
            .enclosed(Limit::new().set_request_body_max_size(chunk.len()))
            .finish()
            .call(())
            .now_or_panic()
            .unwrap()
            .call(req)
            .now_or_panic()
            .ok()
            .unwrap()
            .into_body();

        let body = collect_body(body).now_or_panic().unwrap();

        assert_eq!(body, chunk);
    }
}
