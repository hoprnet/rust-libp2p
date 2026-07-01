// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Implementation of the libp2p-specific [mDNS](https://github.com/libp2p/specs/blob/master/discovery/mdns.md) protocol.
//!
//! mDNS is a protocol defined by [RFC 6762](https://tools.ietf.org/html/rfc6762) that allows
//! querying nodes that correspond to a certain domain name.
//!
//! In the context of libp2p, the mDNS protocol is used to discover other nodes on the local
//! network that support libp2p.
//!
//! # Usage
//!
//! This crate provides `TokioMdns`
//! which implements the `NetworkBehaviour` trait. This struct will automatically discover other
//! libp2p nodes on the local network.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::{
    fmt, io,
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

mod behaviour;
#[cfg(feature = "tokio")]
pub use crate::behaviour::tokio;
pub use crate::behaviour::{Behaviour, Event};
pub use socket2;

/// The DNS service name for all libp2p peers used to query for addresses.
const SERVICE_NAME: &[u8] = b"_p2p._udp.local";
/// `SERVICE_NAME` as a Fully Qualified Domain Name.
const SERVICE_NAME_FQDN: &str = "_p2p._udp.local.";
/// The meta query for looking up the `SERVICE_NAME`.
const META_QUERY_SERVICE: &[u8] = b"_services._dns-sd._udp.local";
/// `META_QUERY_SERVICE` as a Fully Qualified Domain Name.
const META_QUERY_SERVICE_FQDN: &str = "_services._dns-sd._udp.local.";

pub const IPV4_MDNS_MULTICAST_ADDRESS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const IPV6_MDNS_MULTICAST_ADDRESS: Ipv6Addr = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 0xFB);

/// Callback applied to every UDP socket the mDNS behaviour creates, allowing the caller to
/// set arbitrary socket options (e.g. `SO_MARK`) before the socket is bound.
pub(crate) type SocketConfigFn = Arc<dyn Fn(&socket2::Socket) -> io::Result<()> + Send + Sync>;

/// Wrapper around a [`SocketConfigFn`] so that [`Config`] keeps deriving [`Clone`] and
/// [`Debug`].
///
/// This type is opaque; construct it via [`Config::with_socket_config`].
#[derive(Clone)]
pub struct SocketConfig(pub(crate) SocketConfigFn);

impl fmt::Debug for SocketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SocketConfig(<fn>)")
    }
}

/// Configuration for mDNS.
#[derive(Debug, Clone)]
pub struct Config {
    /// TTL to use for mdns records.
    pub ttl: Duration,
    /// Interval at which to poll the network for new peers. This isn't
    /// necessary during normal operation but avoids the case that an
    /// initial packet was lost and not discovering any peers until a new
    /// peer joins the network. Receiving an mdns packet resets the timer
    /// preventing unnecessary traffic.
    pub query_interval: Duration,
    /// Use IPv6 instead of IPv4.
    pub enable_ipv6: bool,
    /// Optional callback to customise UDP sockets before they are bound.
    ///
    /// This field is opaque; set it via [`Config::with_socket_config`].
    pub socket_config: Option<SocketConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(6 * 60),
            query_interval: Duration::from_secs(5 * 60),
            enable_ipv6: false,
            socket_config: None,
        }
    }
}

impl Config {
    /// Registers a callback that is run against every UDP socket the mDNS behaviour
    /// creates, *before* the socket is bound.
    ///
    /// This grants access to the underlying [`socket2::Socket`] so that arbitrary socket
    /// options can be set. Returning an error aborts creation of that socket.
    ///
    /// A common use is tagging sockets with `SO_MARK` so that firewall rules can classify
    /// or filter libp2p traffic via fwmark:
    ///
    /// ```no_run
    /// # use libp2p_mdns::Config;
    /// let config = Config::default().with_socket_config(|socket| {
    ///     #[cfg(target_os = "linux")]
    ///     socket.set_mark(0x1234)?;
    ///     Ok(())
    /// });
    /// ```
    pub fn with_socket_config(
        mut self,
        f: impl Fn(&socket2::Socket) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.socket_config = Some(SocketConfig(Arc::new(f)));
        self
    }
}
