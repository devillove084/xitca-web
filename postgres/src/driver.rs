pub(crate) mod codec;
pub(crate) mod generic;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "io-uring")]
mod io_uring;

#[cfg(not(feature = "quic"))]
mod raw;

#[cfg(not(feature = "quic"))]
pub(crate) use raw::*;

#[cfg(feature = "quic")]
mod quic;
#[cfg(feature = "quic")]
pub(crate) use quic::*;

use core::{
    future::{Future, IntoFuture},
    pin::Pin,
};

use std::net::SocketAddr;

use postgres_protocol::message::backend;
use xitca_io::bytes::BytesMut;

use super::{client::Client, config::Config, error::Error, iter::AsyncIterator};

#[cfg(not(feature = "quic"))]
use {self::generic::GenericDriver, xitca_io::net::TcpStream};

#[cfg(not(feature = "quic"))]
#[cfg(feature = "tls")]
use xitca_tls::rustls::{ClientConnection, TlsStream};

#[cfg(not(feature = "quic"))]
#[cfg(unix)]
use xitca_io::net::UnixStream;

pub(super) async fn connect(mut cfg: Config) -> Result<(Client, Driver), Error> {
    let mut err = None;
    let hosts = cfg.get_hosts().to_vec();
    for host in hosts {
        match _connect(host, &mut cfg).await {
            Ok(t) => return Ok(t),
            Err(e) => err = Some(e),
        }
    }

    Err(err.unwrap())
}

/// async driver of [Client](crate::Client).
/// it handles IO and emit server sent message that do not belong to any query with [AsyncIterator]
/// trait impl.
///
/// # Examples:
/// ```rust
/// use std::future::IntoFuture;
/// use xitca_postgres::{Driver, AsyncIterator};
///
/// // drive the client and listen to server notify at the same time.
/// fn drive_with_server_notify(mut drv: Driver) {
///     tokio::spawn(async move {
///         while let Some(Ok(msg)) = drv.next().await {
///             // *Note:
///             // handle message must be non-blocking to prevent starvation of driver.
///         }
///     });
/// }
///
/// // drive client without handling notify.
/// fn drive_only(drv: Driver) {
///     tokio::spawn(drv.into_future());
/// }
/// ```
pub struct Driver {
    inner: _Driver,
}

#[cfg(not(feature = "quic"))]
impl Driver {
    pub(super) fn tcp(drv: GenericDriver<TcpStream>) -> Self {
        Self {
            inner: _Driver::Tcp(drv),
        }
    }

    #[cfg(feature = "tls")]
    pub(super) fn tls(drv: GenericDriver<TlsStream<ClientConnection, TcpStream>>) -> Self {
        Self {
            inner: _Driver::Tls(drv),
        }
    }

    #[cfg(unix)]
    pub(super) fn unix(drv: GenericDriver<UnixStream>) -> Self {
        Self {
            inner: _Driver::Unix(drv),
        }
    }

    #[cfg(all(unix, feature = "tls"))]
    pub(super) fn unix_tls(drv: GenericDriver<TlsStream<ClientConnection, UnixStream>>) -> Self {
        Self {
            inner: _Driver::UnixTls(drv),
        }
    }
}

#[cfg(feature = "quic")]
impl Driver {
    pub(super) fn quic(drv: QuicDriver) -> Self {
        Self {
            inner: _Driver::Quic(drv),
        }
    }
}

// TODO: use Box<dyn AsyncIterator> when life time GAT is object safe.
#[cfg(not(feature = "quic"))]
enum _Driver {
    Tcp(GenericDriver<TcpStream>),
    #[cfg(feature = "tls")]
    Tls(GenericDriver<TlsStream<ClientConnection, TcpStream>>),
    #[cfg(unix)]
    Unix(GenericDriver<UnixStream>),
    #[cfg(all(unix, feature = "tls"))]
    UnixTls(GenericDriver<TlsStream<ClientConnection, UnixStream>>),
}

#[cfg(feature = "quic")]
enum _Driver {
    Quic(QuicDriver),
}

impl AsyncIterator for Driver {
    type Item<'i> = Result<backend::Message, Error> where Self: 'i;

    #[inline]
    async fn next(&mut self) -> Option<Self::Item<'_>> {
        #[cfg(not(feature = "quic"))]
        match self.inner {
            _Driver::Tcp(ref mut drv) => drv.next().await,
            #[cfg(feature = "tls")]
            _Driver::Tls(ref mut drv) => drv.next().await,
            #[cfg(unix)]
            _Driver::Unix(ref mut drv) => drv.next().await,
            #[cfg(all(unix, feature = "tls"))]
            _Driver::UnixTls(ref mut drv) => drv.next().await,
        }

        #[cfg(feature = "quic")]
        match self.inner {
            _Driver::Quic(ref mut drv) => drv.next().await,
        }
    }
}

impl IntoFuture for Driver {
    type Output = Result<(), Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        #[cfg(not(feature = "quic"))]
        match self.inner {
            _Driver::Tcp(drv) => Box::pin(drv.run()),
            #[cfg(feature = "tls")]
            _Driver::Tls(drv) => Box::pin(drv.run()),
            #[cfg(unix)]
            _Driver::Unix(drv) => Box::pin(drv.run()),
            #[cfg(all(unix, feature = "tls"))]
            _Driver::UnixTls(drv) => Box::pin(drv.run()),
        }

        #[cfg(feature = "quic")]
        match self.inner {
            _Driver::Quic(drv) => Box::pin(drv.run()),
        }
    }
}

async fn resolve(host: &str, ports: &[u16]) -> Result<Vec<SocketAddr>, Error> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await?
        .flat_map(|mut addr| {
            ports.iter().map(move |port| {
                addr.set_port(*port);
                addr
            })
        })
        .collect::<Vec<_>>();
    Ok(addrs)
}

#[cfg(feature = "io-uring")]
impl Driver {
    // downcast Driver to IoUringDriver if it's Tcp variant.
    // IoUringDriver can not be a new variant of Dirver as it's !Send.
    pub fn try_into_io_uring_tcp(self) -> io_uring::IoUringDriver<xitca_io::net::io_uring::TcpStream> {
        #[cfg(not(feature = "quic"))]
        match self.inner {
            _Driver::Tcp(drv) => {
                let std = drv.io.into_std().unwrap();
                let tcp = xitca_io::net::io_uring::TcpStream::from_std(std);
                io_uring::IoUringDriver::new(
                    tcp,
                    drv.rx.unwrap(),
                    drv.write_buf.into_inner(),
                    drv.read_buf.into_inner(),
                    drv.res,
                )
            }
            _ => todo!(),
        }

        #[cfg(feature = "quic")]
        todo!()
    }
}

pub(crate) trait Drive: Send {
    fn send(&mut self, msg: BytesMut) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>>;

    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Result<backend::Message, Error>> + Send + '_>>;
}
