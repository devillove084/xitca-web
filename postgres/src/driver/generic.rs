use core::{
    convert::Infallible,
    future::{poll_fn, Future},
    pin::Pin,
};

use alloc::collections::VecDeque;

use std::io;

use postgres_protocol::message::backend;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::error;
use xitca_io::{
    bytes::{BufInterest, BufRead, BufWrite, BytesMut, WriteBuf},
    io::{AsyncIo, Interest},
};
use xitca_unsafe_collection::futures::{Select as _, SelectOutput};

use crate::{error::Error, iter::AsyncIterator};

use super::{
    codec::{Request, ResponseMessage, ResponseSender},
    Drive,
};

type PagedBytesMut = xitca_unsafe_collection::bytes::PagedBytesMut<4096>;

pub(crate) type GenericDriverTx = UnboundedSender<Request>;
pub(crate) type GenericDriverRx = UnboundedReceiver<Request>;

pub(crate) struct GenericDriver<Io> {
    pub(crate) io: Io,
    pub(crate) write_buf: WriteBuf,
    pub(crate) read_buf: PagedBytesMut,
    pub(crate) rx: Option<GenericDriverRx>,
    pub(crate) res: VecDeque<ResponseSender>,
}

impl<Io> GenericDriver<Io>
where
    Io: AsyncIo,
{
    pub(crate) fn new(io: Io) -> (Self, GenericDriverTx) {
        let (tx, rx) = unbounded_channel();
        (
            Self {
                io,
                write_buf: WriteBuf::new(),
                read_buf: PagedBytesMut::new(),
                rx: Some(rx),
                res: VecDeque::new(),
            },
            tx,
        )
    }

    pub(crate) async fn try_next(&mut self) -> Result<Option<backend::Message>, Error> {
        loop {
            if let Some(msg) = self.try_decode()? {
                return Ok(Some(msg));
            }

            let interest = if self.write_buf.want_write_io() {
                Interest::READABLE | Interest::WRITABLE
            } else {
                Interest::READABLE
            };

            let select = match self.rx {
                Some(ref mut rx) => {
                    let ready = self.io.ready(interest);
                    rx.recv().select(ready).await
                }
                None => {
                    if !interest.is_writable() && self.res.is_empty() {
                        // no interest to write to io and all response have been finished so
                        // shutdown io and exit.
                        // if there is a better way to exhaust potential remaining backend message
                        // please file an issue.
                        poll_fn(|cx| Pin::new(&mut self.io).poll_shutdown(cx)).await?;
                        return Ok(None);
                    }
                    let ready = self.io.ready(interest);
                    SelectOutput::B(ready.await)
                }
            };

            match select {
                // batch message and keep polling.
                SelectOutput::A(Some(req)) => {
                    self.write_buf_extend(req.msg.as_ref());
                    self.res.push_back(req.tx);
                }
                SelectOutput::B(ready) => {
                    let ready = ready?;
                    if ready.is_readable() {
                        self.try_read()?;
                    }
                    if ready.is_writable() && self.try_write().is_err() {
                        // write failed as server stopped reading.
                        // drop channel so all pending request in it can be notified.
                        self.rx = None;
                    }
                }
                SelectOutput::A(None) => self.rx = None,
            }
        }
    }

    // TODO: remove this feature gate.
    #[cfg(not(feature = "quic"))]
    pub(crate) async fn run(mut self) -> Result<(), Error> {
        while self.try_next().await?.is_some() {}
        Ok(())
    }

    pub(crate) async fn send(&mut self, msg: BytesMut) -> Result<(), Error> {
        self.write_buf_extend(&msg);
        loop {
            self.try_write()?;
            if self.write_buf.is_empty() {
                return Ok(());
            }
            let ready = self.io.ready(Interest::WRITABLE);
            ready.await?;
        }
    }

    pub(crate) async fn recv_with<F, O>(&mut self, mut func: F) -> Result<O, Error>
    where
        F: FnMut(&mut BytesMut) -> Option<Result<O, Error>>,
    {
        loop {
            if let Some(o) = func(self.read_buf.get_mut()) {
                return o;
            }
            let ready = self.io.ready(Interest::READABLE);
            ready.await?;
            self.try_read()?;
        }
    }

    fn write_buf_extend(&mut self, buf: &[u8]) {
        let _ = self.write_buf.write_buf(|w| {
            w.extend_from_slice(buf);
            Ok::<_, Infallible>(())
        });
    }

    fn try_read(&mut self) -> Result<(), Error> {
        self.read_buf.do_io(&mut self.io).map_err(Into::into)
    }

    fn try_write(&mut self) -> io::Result<()> {
        self.write_buf.do_io(&mut self.io).map_err(|e| {
            // when write error occur the driver would go into half close state(read only).
            // clearing write_buf would drop all pending requests in it and hint the driver no
            // future Interest::READABLE should be passed to AsyncIo::ready method.
            self.write_buf.clear();
            error!("server closed read half unexpectedly: {e}");
            e
        })
    }

    fn try_decode(&mut self) -> Result<Option<backend::Message>, Error> {
        while let Some(res) = ResponseMessage::try_from_buf(self.read_buf.get_mut())? {
            match res {
                ResponseMessage::Normal { buf, complete } => {
                    let front = self.res.front_mut().expect("out of bound must not happen");
                    front.send(buf);
                    if front.complete(complete) {
                        self.res.pop_front();
                    }
                }
                ResponseMessage::Async(msg) => return Ok(Some(msg)),
            }
        }
        Ok(None)
    }
}

impl<Io> AsyncIterator for GenericDriver<Io>
where
    Io: AsyncIo + Send,
{
    type Item<'i> = Result<backend::Message, Error> where Self: 'i;

    #[inline]
    async fn next(&mut self) -> Option<Self::Item<'_>> {
        self.try_next().await.transpose()
    }
}

impl<Io> Drive for GenericDriver<Io>
where
    Io: AsyncIo + Send,
{
    fn send(&mut self, msg: BytesMut) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>> {
        Box::pin(self.send(msg))
    }

    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Result<backend::Message, Error>> + Send + '_>> {
        Box::pin(self.recv_with(|buf| backend::Message::parse(buf).map_err(Error::from).transpose()))
    }
}
