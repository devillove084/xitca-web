use core::convert::Infallible;

use std::{error, io};

use crate::{
    body::BodyStream,
    bytes::Bytes,
    context::WebContext,
    error::{MatchError, MethodNotAllowed},
    http::{
        const_header_value::TEXT_UTF8,
        header::{ALLOW, CONTENT_TYPE},
        StatusCode, WebResponse,
    },
};

use super::{error::ExtractError, FromRequest, Responder};

impl<'a, 'r, C, B, T, E> FromRequest<'a, WebContext<'r, C, B>> for Result<T, E>
where
    B: BodyStream,
    T: for<'a2, 'r2> FromRequest<'a2, WebContext<'r2, C, B>, Error = E>,
{
    type Type<'b> = Result<T, E>;
    type Error = ExtractError<B::Error>;

    #[inline]
    async fn from_request(ctx: &'a WebContext<'r, C, B>) -> Result<Self, Self::Error> {
        Ok(T::from_request(ctx).await)
    }
}

impl<'a, 'r, C, B, T> FromRequest<'a, WebContext<'r, C, B>> for Option<T>
where
    B: BodyStream,
    T: for<'a2, 'r2> FromRequest<'a2, WebContext<'r2, C, B>>,
{
    type Type<'b> = Option<T>;
    type Error = ExtractError<B::Error>;

    #[inline]
    async fn from_request(ctx: &'a WebContext<'r, C, B>) -> Result<Self, Self::Error> {
        Ok(T::from_request(ctx).await.ok())
    }
}

impl<'a, 'r, C, B> FromRequest<'a, WebContext<'r, C, B>> for &'a WebContext<'a, C, B>
where
    C: 'static,
    B: BodyStream + 'static,
{
    type Type<'b> = &'b WebContext<'b, C, B>;
    type Error = ExtractError<B::Error>;

    #[inline]
    async fn from_request(ctx: &'a WebContext<'r, C, B>) -> Result<Self, Self::Error> {
        Ok(ctx)
    }
}

impl<'a, 'r, C, B> FromRequest<'a, WebContext<'r, C, B>> for ()
where
    B: BodyStream,
{
    type Type<'b> = ();
    type Error = ExtractError<B::Error>;

    #[inline]
    async fn from_request(_: &'a WebContext<'r, C, B>) -> Result<Self, Self::Error> {
        Ok(())
    }
}

impl<'r, C, B, T, O, E> Responder<WebContext<'r, C, B>> for Result<T, E>
where
    T: for<'r2> Responder<WebContext<'r2, C, B>, Output = O>,
{
    type Output = Result<O, E>;

    #[inline]
    async fn respond_to(self, ctx: WebContext<'r, C, B>) -> Self::Output {
        Ok(self?.respond_to(ctx).await)
    }
}

impl<'r, C, B, ResB> Responder<WebContext<'r, C, B>> for WebResponse<ResB> {
    type Output = WebResponse<ResB>;

    #[inline]
    async fn respond_to(self, _: WebContext<'r, C, B>) -> Self::Output {
        self
    }
}

impl<'r, C, B> Responder<WebContext<'r, C, B>> for Infallible {
    type Output = WebResponse;

    async fn respond_to(self, _: WebContext<'r, C, B>) -> Self::Output {
        match self {}
    }
}

macro_rules! text_utf8 {
    ($type: ty) => {
        impl<'r, C, B> Responder<WebContext<'r, C, B>> for $type {
            type Output = WebResponse;

            async fn respond_to(self, ctx: WebContext<'r, C, B>) -> Self::Output {
                let mut res = ctx.into_response(self);
                res.headers_mut().insert(CONTENT_TYPE, TEXT_UTF8);
                res
            }
        }
    };
}

text_utf8!(String);
text_utf8!(&'static str);

macro_rules! blank_internal {
    ($type: ty) => {
        impl<'r, C, B> Responder<WebContext<'r, C, B>> for $type {
            type Output = WebResponse;

            async fn respond_to(self, ctx: WebContext<'r, C, B>) -> Self::Output {
                let mut res = ctx.into_response(Bytes::new());
                *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                res
            }
        }
    };
}

blank_internal!(io::Error);
blank_internal!(Box<dyn error::Error>);
blank_internal!(Box<dyn error::Error + Send>);
blank_internal!(Box<dyn error::Error + Send + Sync>);

impl<'r, C, B> Responder<WebContext<'r, C, B>> for MatchError {
    type Output = WebResponse;

    async fn respond_to(self, ctx: WebContext<'r, C, B>) -> Self::Output {
        let mut res = ctx.into_response(Bytes::new());
        *res.status_mut() = StatusCode::NOT_FOUND;
        res
    }
}

impl<'r, C, B> Responder<WebContext<'r, C, B>> for MethodNotAllowed {
    type Output = WebResponse;

    async fn respond_to(self, ctx: WebContext<'r, C, B>) -> Self::Output {
        let mut res = ctx.into_response(Bytes::new());

        let allowed = self.allowed_methods();

        let len = allowed.iter().fold(0, |a, m| a + m.as_str().len() + 1);

        let mut methods = String::with_capacity(len);

        for method in allowed {
            methods.push_str(method.as_str());
            methods.push(',');
        }
        methods.pop();

        res.headers_mut().insert(ALLOW, methods.parse().unwrap());
        *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;

        res
    }
}

#[cfg(test)]
mod test {
    use xitca_http::BodyError;
    use xitca_unsafe_collection::futures::NowOrPanic;

    use super::*;

    #[test]
    fn extract_default_impls() {
        let mut req = WebContext::new_test(());
        let req = req.as_web_ctx();

        Option::<()>::from_request(&req).now_or_panic().unwrap().unwrap();

        Result::<(), ExtractError<BodyError>>::from_request(&req)
            .now_or_panic()
            .unwrap()
            .unwrap();

        <&WebContext<'_>>::from_request(&req).now_or_panic().unwrap();

        <()>::from_request(&req).now_or_panic().unwrap();
    }
}
