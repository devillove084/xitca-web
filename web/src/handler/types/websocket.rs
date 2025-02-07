use core::{
    future::{poll_fn, Future},
    pin::{pin, Pin},
    time::Duration,
};

use futures_core::stream::Stream;
use http_ws::{
    stream::{RequestStream, ResponseSender, WsError},
    HandshakeError, Item, Message as WsMessage, WsOutput,
};
use tokio::time::{sleep, Instant};
use xitca_unsafe_collection::{
    bytes::BytesStr,
    futures::{Select, SelectOutput},
};

use crate::{
    body::{BodyStream, RequestBody, ResponseBody},
    bytes::Bytes,
    context::WebContext,
    handler::{error::ExtractError, FromRequest, Responder},
    http::{
        header::{CONNECTION, SEC_WEBSOCKET_VERSION, UPGRADE},
        WebResponse,
    },
};

/// simplified websocket message type.
/// for more variant of message please reference [http_ws::Message] type.
#[derive(Debug, Eq, PartialEq)]
pub enum Message {
    Text(BytesStr),
    Binary(Bytes),
    Continuation(Item),
}

type BoxFuture<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

type OnMsgCB = Box<dyn for<'a> FnMut(&'a mut ResponseSender, Message) -> BoxFuture<'a>>;

type OnErrCB<E> = Box<dyn FnMut(WsError<E>) -> BoxFuture<'static>>;

type OnCloseCB = Box<dyn FnOnce() -> BoxFuture<'static>>;

pub struct WebSocket<B = RequestBody>
where
    B: BodyStream,
{
    ws: WsOutput<B, B::Error>,
    ping_interval: Duration,
    max_unanswered_ping: u8,
    on_msg: OnMsgCB,
    on_err: OnErrCB<B::Error>,
    on_close: OnCloseCB,
}

impl<B> WebSocket<B>
where
    B: BodyStream,
{
    fn new(ws: WsOutput<B, B::Error>) -> Self {
        #[cold]
        #[inline(never)]
        fn boxed_future() -> BoxFuture<'static> {
            Box::pin(async {})
        }

        Self {
            ws,
            ping_interval: Duration::from_secs(15),
            max_unanswered_ping: 3,
            on_msg: Box::new(|_, _| boxed_future()),
            on_err: Box::new(|_| boxed_future()),
            on_close: Box::new(|| boxed_future()),
        }
    }

    /// Set interval duration of server side ping message to client.
    pub fn set_ping_interval(&mut self, dur: Duration) -> &mut Self {
        self.ping_interval = dur;
        self
    }

    /// Set max number of consecutive server side ping messages that are not
    /// answered by client.
    ///
    /// # Panic:
    /// when 0 is passed as argument.
    pub fn set_max_unanswered_ping(&mut self, size: u8) -> &mut Self {
        assert!(size > 0, "max_unanswered_ping MUST be none 0");
        self.max_unanswered_ping = size;
        self
    }

    /// Get a reference of Websocket message sender.
    /// Can be used to send message to client.
    pub fn msg_sender(&self) -> &ResponseSender {
        &self.ws.2
    }

    /// Async function that would be called when new message arrived from client.
    pub fn on_msg<F>(&mut self, func: F) -> &mut Self
    where
        F: for<'a> FnMut(&'a mut ResponseSender, Message) -> BoxFuture<'a> + 'static,
    {
        self.on_msg = Box::new(func);
        self
    }

    /// Async function that would be called when error occurred.
    pub fn on_err<F, Fut>(&mut self, mut func: F) -> &mut Self
    where
        F: FnMut(WsError<B::Error>) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.on_err = Box::new(move |e| Box::pin(func(e)));
        self
    }

    /// Async function that would be called when closing the websocket connection.
    pub fn on_close<F, Fut>(&mut self, func: F) -> &mut Self
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.on_close = Box::new(|| Box::pin(func()));
        self
    }
}

impl<E> From<HandshakeError> for ExtractError<E> {
    fn from(e: HandshakeError) -> Self {
        match e {
            HandshakeError::NoConnectionUpgrade => ExtractError::HeaderNotFound(CONNECTION),
            HandshakeError::NoVersionHeader => ExtractError::HeaderNotFound(SEC_WEBSOCKET_VERSION),
            HandshakeError::NoWebsocketUpgrade => ExtractError::HeaderNotFound(UPGRADE),
            // TODO: refine error mapping of the remaining branches.
            e => ExtractError::Boxed(Box::new(e)),
        }
    }
}

impl<'a, 'r, C, B> FromRequest<'a, WebContext<'r, C, B>> for WebSocket<B>
where
    C: 'static,
    B: BodyStream + Default + 'static,
{
    type Type<'b> = WebSocket<B>;
    type Error = ExtractError<B::Error>;

    #[inline]
    async fn from_request(ctx: &'a WebContext<'r, C, B>) -> Result<Self, Self::Error> {
        let body = ctx.take_body_ref();
        let ws = http_ws::ws(ctx.req(), body)?;
        Ok(WebSocket::new(ws))
    }
}

impl<'r, C, B> Responder<WebContext<'r, C, B>> for WebSocket<B>
where
    B: BodyStream + 'static,
{
    type Output = WebResponse;

    async fn respond_to(self, _: WebContext<'r, C, B>) -> Self::Output {
        let Self {
            ws,
            ping_interval,
            max_unanswered_ping,
            on_msg,
            on_err,
            on_close,
        } = self;

        let (decode, res, tx) = ws;

        tokio::task::spawn_local(spawn_task(
            ping_interval,
            max_unanswered_ping,
            decode,
            tx,
            on_msg,
            on_err,
            on_close,
        ));

        res.map(ResponseBody::box_stream)
    }
}

async fn spawn_task<B>(
    ping_interval: Duration,
    max_unanswered_ping: u8,
    decode: RequestStream<B, B::Error>,
    mut tx: ResponseSender,
    mut on_msg: OnMsgCB,
    mut on_err: OnErrCB<B::Error>,
    on_close: OnCloseCB,
) where
    B: BodyStream,
{
    let on_msg = &mut *on_msg;
    let on_err = &mut *on_err;

    let spawn_inner = || async {
        let mut sleep = pin!(sleep(ping_interval));
        let mut decode = pin!(decode);

        let mut un_answered_ping = 0u8;

        loop {
            match poll_fn(|cx| decode.as_mut().poll_next(cx)).select(sleep.as_mut()).await {
                SelectOutput::A(Some(Ok(msg))) => {
                    let msg = match msg {
                        WsMessage::Pong(_) => {
                            if let Some(num) = un_answered_ping.checked_sub(1) {
                                un_answered_ping = num;
                            }
                            continue;
                        }
                        WsMessage::Ping(ping) => {
                            tx.send(WsMessage::Pong(ping)).await?;
                            continue;
                        }
                        WsMessage::Close(reason) => {
                            tx.send(WsMessage::Close(reason)).await?;
                            break;
                        }
                        WsMessage::Text(txt) => Message::Text(BytesStr::try_from(txt).unwrap()),
                        WsMessage::Binary(bin) => Message::Binary(bin),
                        WsMessage::Continuation(item) => Message::Continuation(item),
                        WsMessage::Nop => continue,
                    };

                    on_msg(&mut tx, msg).await
                }
                SelectOutput::A(Some(Err(e))) => on_err(e).await,
                SelectOutput::A(None) => break,
                SelectOutput::B(_) => {
                    if un_answered_ping > max_unanswered_ping {
                        tx.send(WsMessage::Close(None)).await?;
                        break;
                    } else {
                        un_answered_ping += 1;
                        tx.send(WsMessage::Ping(Bytes::new())).await?;
                        sleep.as_mut().reset(Instant::now() + ping_interval);
                    }
                }
            }
        }

        Ok(())
    };

    if let Err(e) = spawn_inner().await {
        on_err(e).await;
    }

    on_close().await;
}
