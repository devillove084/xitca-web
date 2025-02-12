#![allow(dead_code)]

mod data;
mod dispatcher;
mod error;
mod head;
mod headers;
mod hpack;
mod priority;
mod settings;
mod stream_id;

pub(crate) use dispatcher::Dispatcher;

const HEADER_LEN: usize = 9;

#[cfg(feature = "io-uring")]
pub use io_uring::run;

#[cfg(feature = "io-uring")]
mod io_uring {
    use core::{
        fmt,
        future::Future,
        mem,
        pin::{pin, Pin},
        task::{Context, Poll},
    };

    use std::{collections::HashMap, io};

    use pin_project_lite::pin_project;
    use tracing::error;
    use xitca_io::{
        bytes::{Buf, BufMut, BytesMut},
        io_uring::{write_all, AsyncBufRead, AsyncBufWrite, IoBuf},
    };
    use xitca_service::Service;
    use xitca_unsafe_collection::futures::{Select, SelectOutput};

    use crate::{
        h2::{RequestBodySender, RequestBodyV2},
        http::{Request, RequestExt, Response, Version},
        util::futures::Queue,
    };

    use super::{
        data,
        error::Error,
        head, headers, hpack,
        settings::{self, Settings},
        stream_id::StreamId,
    };

    const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    struct H2Context {
        max_header_list_size: usize,
        decoder: hpack::Decoder,
        encoder: hpack::Encoder,
        tx_map: HashMap<StreamId, RequestBodySender>,
        // next_frame_len == 0 is used as maker for waiting for new frame.
        next_frame_len: usize,
        continuation: Option<(headers::Headers, BytesMut)>,
    }

    impl H2Context {
        fn new(local_setting: Settings) -> Self {
            Self {
                max_header_list_size: local_setting
                    .max_header_list_size()
                    .map(|val| val as _)
                    .unwrap_or(settings::DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE),
                decoder: hpack::Decoder::new(settings::DEFAULT_SETTINGS_HEADER_TABLE_SIZE),
                encoder: hpack::Encoder::new(65535, 4096),
                tx_map: HashMap::new(),
                next_frame_len: 0,
                continuation: None,
            }
        }

        fn try_decode<F>(&mut self, buf: &mut BytesMut, mut on_msg: F) -> Result<(), Error>
        where
            F: FnMut(Request<RequestExt<RequestBodyV2>>, StreamId),
        {
            loop {
                if self.next_frame_len == 0 {
                    if buf.len() < 3 {
                        return Ok(());
                    }
                    self.next_frame_len = (buf.get_uint(3) + 6) as _;
                }

                if buf.len() < self.next_frame_len {
                    return Ok(());
                }

                let len = mem::replace(&mut self.next_frame_len, 0);
                let mut frame = buf.split_to(len);
                let head = head::Head::parse(&frame);

                // TODO: Make Head::parse auto advance the frame?
                frame.advance(6);

                match head.kind() {
                    head::Kind::Settings => {
                        let _setting = settings::Settings::load(head, &frame).unwrap();
                    }
                    head::Kind::Headers => {
                        let (mut headers, mut payload) = headers::Headers::load(head, frame).unwrap();

                        let is_end_headers = headers.is_end_headers();

                        headers
                            .load_hpack(&mut payload, self.max_header_list_size, &mut self.decoder)
                            .unwrap();

                        if !is_end_headers {
                            self.continuation = Some((headers, payload));
                            continue;
                        }

                        let id = headers.stream_id();

                        self.handle_header_frame(id, headers, &mut on_msg);
                    }
                    head::Kind::Continuation => {
                        let is_end_headers = (head.flag() & 0x4) == 0x4;

                        let Some((mut headers, mut payload)) = self.continuation.take() else {
                            panic!("illegal continuation frame");
                        };

                        let id = headers.stream_id();

                        if id != head.stream_id() {
                            panic!("CONTINUATION frame stream ID does not match previous frame stream ID");
                        }

                        payload.extend_from_slice(&frame);

                        if let Err(e) = headers.load_hpack(&mut payload, self.max_header_list_size, &mut self.decoder) {
                            match e {
                                Error::Hpack(hpack::DecoderError::NeedMore(_)) if !is_end_headers => {
                                    self.continuation = Some((headers, payload));
                                    continue;
                                }
                                e => return Err(e),
                            }
                        }

                        self.handle_header_frame(id, headers, &mut on_msg);
                    }
                    head::Kind::Data => {
                        let data = data::Data::load(head, frame.freeze()).unwrap();
                        let is_end = data.is_end_stream();
                        let id = data.stream_id();
                        let payload = data.into_payload();

                        let tx = self.tx_map.get_mut(&id).unwrap();

                        tx.send(Ok(payload)).unwrap();

                        if is_end {
                            self.tx_map.remove(&id);
                        }
                    }
                    _ => {}
                }
            }
        }

        fn handle_header_frame<F>(&mut self, id: StreamId, headers: headers::Headers, on_msg: &mut F)
        where
            F: FnMut(Request<RequestExt<RequestBodyV2>>, StreamId),
        {
            let is_end_stream = headers.is_end_stream();

            let (pseudo, headers) = headers.into_parts();

            let req = match self.tx_map.remove(&id) {
                Some(_) => {
                    error!("trailer is not supported yet");
                    return;
                }
                None => {
                    let mut req = Request::new(RequestExt::<()>::default());
                    *req.version_mut() = Version::HTTP_2;
                    *req.headers_mut() = headers;
                    *req.method_mut() = pseudo.method.unwrap();
                    req
                }
            };

            let (body, tx) = RequestBodyV2::new_pair();

            if is_end_stream {
                drop(tx);
            } else {
                self.tx_map.insert(id, tx);
            };

            let req = req.map(|ext| ext.map_body(|_| body));

            on_msg(req, id);
        }
    }

    async fn read_io(mut buf: BytesMut, io: &impl AsyncBufRead) -> (io::Result<usize>, BytesMut) {
        let len = buf.len();
        let remaining = buf.capacity() - len;
        if remaining < 4096 {
            buf.reserve(4096 - remaining);
        }
        let (res, buf) = io.read(buf.slice(len..)).await;
        (res, buf.into_inner())
    }

    async fn write_io(buf: BytesMut, io: &impl AsyncBufWrite) -> (io::Result<()>, BytesMut) {
        let (res, mut buf) = write_all(io, buf).await;
        buf.clear();
        (res, buf)
    }

    pin_project! {
        #[project = CompleteTaskProj]
        #[project_replace = CompleteTaskReplaceProj]
        enum CompleteTask<F> {
            Task {
                #[pin]
                fut: F
            },
            Idle
        }
    }

    impl<F> Future for CompleteTask<F>
    where
        F: Future,
    {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.project() {
                CompleteTaskProj::Task { fut } => fut.poll(cx),
                CompleteTaskProj::Idle => Poll::Pending,
            }
        }
    }

    /// Experimental h2 http layer.
    pub async fn run<Io, S>(io: Io, service: S) -> io::Result<()>
    where
        Io: AsyncBufRead + AsyncBufWrite,
        S: Service<Request<RequestExt<RequestBodyV2>>, Response = Response<()>>,
        S::Error: fmt::Debug,
    {
        let mut read_buf = BytesMut::new();
        let mut write_buf = BytesMut::new();

        read_buf = prefix_check(&io, read_buf).await?;

        let mut settings = settings::Settings::default();
        settings.set_max_concurrent_streams(Some(256));

        settings.encode(&mut write_buf);
        let (res, buf) = write_io(write_buf, &io).await;
        write_buf = buf;
        res?;

        let mut ctx = H2Context::new(settings);
        let mut queue = Queue::new();

        let mut read_task = pin!(read_io(read_buf, &io));

        loop {
            match read_task.as_mut().select(queue.next()).await {
                SelectOutput::A((res, buf)) => {
                    read_buf = buf;
                    if res? == 0 {
                        break;
                    }

                    let res = ctx.try_decode(&mut read_buf, |req, stream_id| {
                        let s = &service;
                        queue.push(async move { (s.call(req).await, stream_id) });
                    });

                    if let Err(e) = res {
                        panic!("decode error: {e:?}")
                    }

                    read_task.set(read_io(read_buf, &io));
                }
                SelectOutput::B((res, id)) => {
                    let (parts, _) = match res {
                        Ok(res) => res.into_parts(),
                        Err(e) => {
                            error!("service error: {e:?}");
                            continue;
                        }
                    };
                    let pseudo = headers::Pseudo::response(parts.status);
                    let headers = headers::Headers::new(id, pseudo, parts.headers);
                    let mut buf = (&mut write_buf).limit(4096);
                    headers.encode(&mut ctx.encoder, &mut buf);

                    let (res, buf) = write_io(write_buf, &io).await;
                    write_buf = buf;
                    res?;
                }
            }
        }

        Ok(())
    }

    #[cold]
    #[inline(never)]
    async fn prefix_check(io: &impl AsyncBufRead, mut buf: BytesMut) -> io::Result<BytesMut> {
        while buf.len() < PREFACE.len() {
            let (res, b) = read_io(buf, io).await;
            buf = b;
            res?;
        }

        if &buf[..PREFACE.len()] == PREFACE {
            buf.advance(PREFACE.len());
        } else {
            todo!()
        }

        Ok(buf)
    }
}

/// A helper macro that unpacks a sequence of 4 bytes found in the buffer with
/// the given identifier, starting at the given offset, into the given integer
/// type. Obviously, the integer type should be able to support at least 4
/// bytes.
///
/// # Examples
///
/// ```ignore
/// # // We ignore this doctest because the macro is not exported.
/// let buf: [u8; 4] = [0, 0, 0, 1];
/// assert_eq!(1u32, unpack_octets_4!(buf, 0, u32));
/// ```
macro_rules! unpack_octets_4 {
    // TODO: Get rid of this macro
    ($buf:expr, $offset:expr, $tip:ty) => {
        (($buf[$offset + 0] as $tip) << 24)
            | (($buf[$offset + 1] as $tip) << 16)
            | (($buf[$offset + 2] as $tip) << 8)
            | (($buf[$offset + 3] as $tip) << 0)
    };
}

use unpack_octets_4;

#[cfg(test)]
mod tests {
    #[test]
    fn test_unpack_octets_4() {
        let buf: [u8; 4] = [0, 0, 0, 1];
        assert_eq!(1u32, unpack_octets_4!(buf, 0, u32));
    }
}
