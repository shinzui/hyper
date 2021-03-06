//! The `Connect` trait, and supporting types.
//!
//! This module contains:
//!
//! - A default [`HttpConnector`](HttpConnector) that does DNS resolution and
//!   establishes connections over TCP.
//! - The [`Connect`](Connect) trait and related types to build custom connectors.
use std::error::Error as StdError;
use std::mem;

use bytes::{BufMut, Bytes, BytesMut};
use futures::Future;
use http::{uri, Uri};
use tokio_io::{AsyncRead, AsyncWrite};

#[cfg(feature = "runtime")] mod dns;
#[cfg(feature = "runtime")] mod http;
#[cfg(feature = "runtime")] pub use self::http::HttpConnector;

/// Connect to a destination, returning an IO transport.
///
/// A connector receives a [`Destination`](Destination) describing how a
/// connection should be estabilished, and returns a `Future` of the
/// ready connection.
pub trait Connect: Send + Sync {
    /// The connected IO Stream.
    type Transport: AsyncRead + AsyncWrite + Send + 'static;
    /// An error occured when trying to connect.
    type Error: Into<Box<StdError + Send + Sync>>;
    /// A Future that will resolve to the connected Transport.
    type Future: Future<Item=(Self::Transport, Connected), Error=Self::Error> + Send;
    /// Connect to a destination.
    fn connect(&self, dst: Destination) -> Self::Future;
}

/// A set of properties to describe where and how to try to connect.
#[derive(Clone, Debug)]
pub struct Destination {
    //pub(super) alpn: Alpn,
    pub(super) uri: Uri,
}

/// Extra information about the connected transport.
///
/// This can be used to inform recipients about things like if ALPN
/// was used, or if connected to an HTTP proxy.
#[derive(Debug)]
pub struct Connected {
    //alpn: Alpn,
    pub(super) is_proxied: bool,
}

/*TODO: when HTTP1 Upgrades to H2 are added, this will be needed
#[derive(Debug)]
pub(super) enum Alpn {
    Http1,
    //H2,
    //Http1OrH2
}
*/

impl Destination {
    /// Get the protocol scheme.
    #[inline]
    pub fn scheme(&self) -> &str {
        self.uri
            .scheme_part()
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Get the hostname.
    #[inline]
    pub fn host(&self) -> &str {
        self.uri
            .host()
            .unwrap_or("")
    }

    /// Get the port, if specified.
    #[inline]
    pub fn port(&self) -> Option<u16> {
        self.uri.port()
    }

    /// Update the scheme of this destination.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use hyper::client::connect::Destination;
    /// # fn with_dst(mut dst: Destination) {
    /// // let mut dst = some_destination...
    /// // Change from "http://"...
    /// assert_eq!(dst.scheme(), "http");
    ///
    /// // to "ws://"...
    /// dst.set_scheme("ws");
    /// assert_eq!(dst.scheme(), "ws");
    /// # }
    /// ```
    ///
    /// # Error
    ///
    /// Returns an error if the string is not a valid scheme.
    pub fn set_scheme(&mut self, scheme: &str) -> ::Result<()> {
        let scheme = scheme.parse().map_err(::error::Parse::from)?;
        self.update_uri(move |parts| {
            parts.scheme = Some(scheme);
        })
    }

    /// Update the host of this destination.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use hyper::client::connect::Destination;
    /// # fn with_dst(mut dst: Destination) {
    /// // let mut dst = some_destination...
    /// // Change from "hyper.rs"...
    /// assert_eq!(dst.host(), "hyper.rs");
    ///
    /// // to "some.proxy"...
    /// dst.set_host("some.proxy");
    /// assert_eq!(dst.host(), "some.proxy");
    /// # }
    /// ```
    ///
    /// # Error
    ///
    /// Returns an error if the string is not a valid hostname.
    pub fn set_host(&mut self, host: &str) -> ::Result<()> {
        // Prevent any userinfo setting, it's bad!
        if host.contains('@') {
            return Err(::error::Parse::Uri.into());
        }
        let auth = if let Some(port) = self.port() {
            let bytes = Bytes::from(format!("{}:{}", host, port));
            uri::Authority::from_shared(bytes)
                .map_err(::error::Parse::from)?
        } else {
            let auth = host.parse::<uri::Authority>().map_err(::error::Parse::from)?;
            if auth.port().is_some() {
                return Err(::error::Parse::Uri.into());
            }
            auth
        };
        self.update_uri(move |parts| {
            parts.authority = Some(auth);
        })
    }

    /// Update the port of this destination.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use hyper::client::connect::Destination;
    /// # fn with_dst(mut dst: Destination) {
    /// // let mut dst = some_destination...
    /// // Change from "None"...
    /// assert_eq!(dst.port(), None);
    ///
    /// // to "4321"...
    /// dst.set_port(4321);
    /// assert_eq!(dst.port(), Some(4321));
    ///
    /// // Or remove the port...
    /// dst.set_port(None);
    /// assert_eq!(dst.port(), None);
    /// # }
    /// ```
    pub fn set_port<P>(&mut self, port: P)
    where
        P: Into<Option<u16>>,
    {
        self.set_port_opt(port.into());
    }

    fn set_port_opt(&mut self, port: Option<u16>) {
        use std::fmt::Write;

        let auth = if let Some(port) = port {
            let host = self.host();
            // Need space to copy the hostname, plus ':',
            // plus max 5 port digits...
            let cap = host.len() + 1 + 5;
            let mut buf = BytesMut::with_capacity(cap);
            buf.put_slice(host.as_bytes());
            buf.put_u8(b':');
            write!(buf, "{}", port)
                .expect("should have space for 5 digits");

            uri::Authority::from_shared(buf.freeze())
                .expect("valid host + :port should be valid authority")
        } else {
            self.host().parse()
                .expect("valid host without port should be valid authority")
        };

        self.update_uri(move |parts| {
            parts.authority = Some(auth);
        })
            .expect("valid uri should be valid with port");
    }

    fn update_uri<F>(&mut self, f: F) -> ::Result<()>
    where
        F: FnOnce(&mut uri::Parts)
    {
        // Need to store a default Uri while we modify the current one...
        let old_uri = mem::replace(&mut self.uri, Uri::default());
        // However, mutate a clone, so we can revert if there's an error...
        let mut parts: uri::Parts = old_uri.clone().into();

        f(&mut parts);

        match Uri::from_parts(parts) {
            Ok(uri) => {
                self.uri = uri;
                Ok(())
            },
            Err(err) => {
                self.uri = old_uri;
                Err(::error::Parse::from(err).into())
            },
        }
    }

    /*
    /// Returns whether this connection must negotiate HTTP/2 via ALPN.
    pub fn must_h2(&self) -> bool {
        match self.alpn {
            Alpn::Http1 => false,
            Alpn::H2 => true,
        }
    }
    */
}

impl Connected {
    /// Create new `Connected` type with empty metadata.
    pub fn new() -> Connected {
        Connected {
            //alpn: Alpn::Http1,
            is_proxied: false,
        }
    }

    /// Set whether the connected transport is to an HTTP proxy.
    ///
    /// This setting will affect if HTTP/1 requests written on the transport
    /// will have the request-target in absolute-form or origin-form (such as
    /// `GET http://hyper.rs/guide HTTP/1.1` or `GET /guide HTTP/1.1`).
    ///
    /// Default is `false`.
    pub fn proxy(mut self, is_proxied: bool) -> Connected {
        self.is_proxied = is_proxied;
        self
    }

    /*
    /// Set that the connected transport negotiated HTTP/2 as it's
    /// next protocol.
    pub fn h2(mut self) -> Connected {
        self.alpn = Alpn::H2;
        self
    }
    */
}

#[cfg(test)]
mod tests {
    use super::Destination;

    #[test]
    fn test_destination_set_scheme() {
        let mut dst = Destination {
            uri: "http://hyper.rs".parse().expect("initial parse"),
        };

        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");

        dst.set_scheme("https").expect("set https");
        assert_eq!(dst.scheme(), "https");
        assert_eq!(dst.host(), "hyper.rs");

        dst.set_scheme("<im not a scheme//?>").unwrap_err();
        assert_eq!(dst.scheme(), "https", "error doesn't modify dst");
        assert_eq!(dst.host(), "hyper.rs", "error doesn't modify dst");
    }

    #[test]
    fn test_destination_set_host() {
        let mut dst = Destination {
            uri: "http://hyper.rs".parse().expect("initial parse"),
        };

        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), None);

        dst.set_host("seanmonstar.com").expect("set https");
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "seanmonstar.com");
        assert_eq!(dst.port(), None);

        dst.set_host("/im-not a host! >:)").unwrap_err();
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), None, "error doesn't modify dst");

        // Check port isn't snuck into `set_host`.
        dst.set_host("seanmonstar.com:3030").expect_err("set_host sneaky port");
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), None, "error doesn't modify dst");

        // Check userinfo isn't snuck into `set_host`.
        dst.set_host("sean@nope").expect_err("set_host sneaky userinfo");
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), None, "error doesn't modify dst");

        // Allow IPv6 hosts
        dst.set_host("[::1]").expect("set_host with IPv6");
        assert_eq!(dst.host(), "::1");
        assert_eq!(dst.port(), None, "IPv6 didn't affect port");

        // However, IPv6 with a port is rejected.
        dst.set_host("[::2]:1337").expect_err("set_host with IPv6 and sneaky port");
        assert_eq!(dst.host(), "::1");
        assert_eq!(dst.port(), None);

        // -----------------

        // Also test that an exist port is set correctly.
        let mut dst = Destination {
            uri: "http://hyper.rs:8080".parse().expect("initial parse 2"),
        };

        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), Some(8080));

        dst.set_host("seanmonstar.com").expect("set host");
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "seanmonstar.com");
        assert_eq!(dst.port(), Some(8080));

        dst.set_host("/im-not a host! >:)").unwrap_err();
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), Some(8080), "error doesn't modify dst");

        // Check port isn't snuck into `set_host`.
        dst.set_host("seanmonstar.com:3030").expect_err("set_host sneaky port");
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), Some(8080), "error doesn't modify dst");

        // Check userinfo isn't snuck into `set_host`.
        dst.set_host("sean@nope").expect_err("set_host sneaky userinfo");
        assert_eq!(dst.scheme(), "http", "error doesn't modify dst");
        assert_eq!(dst.host(), "seanmonstar.com", "error doesn't modify dst");
        assert_eq!(dst.port(), Some(8080), "error doesn't modify dst");

        // Allow IPv6 hosts
        dst.set_host("[::1]").expect("set_host with IPv6");
        assert_eq!(dst.host(), "::1");
        assert_eq!(dst.port(), Some(8080), "IPv6 didn't affect port");

        // However, IPv6 with a port is rejected.
        dst.set_host("[::2]:1337").expect_err("set_host with IPv6 and sneaky port");
        assert_eq!(dst.host(), "::1");
        assert_eq!(dst.port(), Some(8080));
    }

    #[test]
    fn test_destination_set_port() {
        let mut dst = Destination {
            uri: "http://hyper.rs".parse().expect("initial parse"),
        };

        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), None);

        dst.set_port(None);
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), None);

        dst.set_port(8080);
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), Some(8080));

        // Also test that an exist port is set correctly.
        let mut dst = Destination {
            uri: "http://hyper.rs:8080".parse().expect("initial parse 2"),
        };

        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), Some(8080));

        dst.set_port(3030);
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), Some(3030));

        dst.set_port(None);
        assert_eq!(dst.scheme(), "http");
        assert_eq!(dst.host(), "hyper.rs");
        assert_eq!(dst.port(), None);
    }
}

