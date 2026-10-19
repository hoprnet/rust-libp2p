#![cfg(feature = "tokio")]

//! End-to-end tests for the BBRv2 congestion controller.
//!
//! These drive real QUIC connections, so they cover what the unit tests of
//! [`libp2p_quic::bbr2`] cannot: that Quinn accepts the [`ControllerFactory`] we
//! hand it and that connections keep working with BBRv2 in charge of the
//! congestion window.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use futures::{future, AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p_core::{
    muxing::{StreamMuxerBox, StreamMuxerExt},
    transport::{Boxed, DialOpts, ListenerId, PortUse, TransportEvent},
    Endpoint, Multiaddr, Transport,
};
use libp2p_identity::PeerId;
use libp2p_quic as quic;
use quic::bbr2::{Bbr2, Bbr2Config, Bbr2Params};
use quinn_proto::congestion::{Controller, ControllerFactory};

const PAYLOAD: &[u8] = b"BBRv2 congestion control over libp2p QUIC";

type BoxedTransport = Boxed<(PeerId, StreamMuxerBox)>;

/// A congestion controller factory that counts how many controllers Quinn built
/// from it, delegating to BBRv2 so that connections keep working.
struct CountingBbr2Factory {
    builds: Arc<AtomicUsize>,
    inner: Bbr2Config,
}

impl ControllerFactory for CountingBbr2Factory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Box::new(Bbr2::new(Arc::new(self.inner.clone()), now, current_mtu))
    }
}

fn create_transport(
    with_config: impl FnOnce(quic::Config) -> quic::Config,
) -> (PeerId, BoxedTransport) {
    let keypair = libp2p_identity::Keypair::generate_ed25519();
    let peer_id = keypair.public().to_peer_id();
    let transport = quic::tokio::Transport::new(with_config(quic::Config::new(&keypair)))
        .map(|(peer_id, connection), _| (peer_id, StreamMuxerBox::new(connection)))
        .boxed();

    (peer_id, transport)
}

async fn start_listening(transport: &mut BoxedTransport, addr: &str) -> Multiaddr {
    transport
        .listen_on(ListenerId::next(), addr.parse().expect("valid multiaddr"))
        .expect("listen_on");

    match transport.next().await {
        Some(TransportEvent::NewAddress { listen_addr, .. }) => listen_addr,
        event => panic!("unexpected transport event: {event:?}"),
    }
}

/// Establish a connection between `listener` and `dialer` and return both ends.
async fn connect(
    listener: &mut BoxedTransport,
    dialer: &mut BoxedTransport,
    addr: Multiaddr,
) -> (StreamMuxerBox, StreamMuxerBox) {
    let accept = async {
        let upgrade = listener
            .select_next_some()
            .await
            .into_incoming()
            .expect("incoming transport event")
            .0;

        upgrade.await.expect("inbound connection").1
    };

    let dial = async {
        dialer
            .dial(
                addr,
                DialOpts {
                    role: Endpoint::Dialer,
                    port_use: PortUse::Reuse,
                },
            )
            .expect("dial")
            .await
            .expect("outbound connection")
            .1
    };

    future::join(accept, dial).await
}

/// Open a substream on `opener`, write the payload, and have `answerer` echo it
/// back.
async fn round_trip(mut opener: StreamMuxerBox, mut answerer: StreamMuxerBox) {
    let open = async {
        let mut stream = future::poll_fn(|cx| {
            let _ = opener.poll_unpin(cx)?;
            opener.poll_outbound_unpin(cx)
        })
        .await
        .expect("outbound substream");

        stream.write_all(PAYLOAD).await.expect("write payload");
        stream.flush().await.expect("flush payload");

        let mut echoed = vec![0; PAYLOAD.len()];
        stream.read_exact(&mut echoed).await.expect("read echo");
        assert_eq!(echoed, PAYLOAD, "payload must survive the round trip");
    };

    let answer = async {
        let mut stream = future::poll_fn(|cx| {
            let _ = answerer.poll_unpin(cx)?;
            answerer.poll_inbound_unpin(cx)
        })
        .await
        .expect("inbound substream");

        let mut received = vec![0; PAYLOAD.len()];
        stream
            .read_exact(&mut received)
            .await
            .expect("read payload");
        assert_eq!(received, PAYLOAD);

        stream.write_all(&received).await.expect("write echo");
        stream.flush().await.expect("flush echo");
    };

    future::join(open, answer).await;
}

#[tokio::test]
async fn bbr2_connections_complete_a_round_trip() {
    let (_, mut listener) = create_transport(quic::Config::with_bbr2);
    let (_, mut dialer) = create_transport(quic::Config::with_bbr2);

    let addr = start_listening(&mut listener, "/ip4/127.0.0.1/udp/0/quic-v1").await;
    let (listener_connection, dialer_connection) = connect(&mut listener, &mut dialer, addr).await;

    round_trip(dialer_connection, listener_connection).await;
}

#[tokio::test]
async fn bbr2_connections_complete_a_round_trip_with_custom_parameters() {
    let (_, mut listener) =
        create_transport(|config| config.with_bbr2_params(Bbr2Params::default()));
    let (_, mut dialer) = create_transport(|config| {
        let params = Bbr2Params {
            probe_rtt_duration: Some(Duration::from_millis(150)),
            ..Default::default()
        };
        config.with_bbr2_params(params)
    });

    let addr = start_listening(&mut listener, "/ip4/127.0.0.1/udp/0/quic-v1").await;
    let (listener_connection, dialer_connection) = connect(&mut listener, &mut dialer, addr).await;

    round_trip(dialer_connection, listener_connection).await;
}

#[tokio::test]
async fn congestion_controller_factory_configured_on_the_transport_is_used() {
    let listener_builds = Arc::new(AtomicUsize::new(0));
    let dialer_builds = Arc::new(AtomicUsize::new(0));

    let (_, mut listener) = create_transport(|config| {
        config.with_congestion_controller_factory(Arc::new(CountingBbr2Factory {
            builds: Arc::clone(&listener_builds),
            inner: Bbr2Config::default(),
        }))
    });
    let (_, mut dialer) = create_transport(|config| {
        config.with_congestion_controller_factory(Arc::new(CountingBbr2Factory {
            builds: Arc::clone(&dialer_builds),
            inner: Bbr2Config::default(),
        }))
    });

    let addr = start_listening(&mut listener, "/ip4/127.0.0.1/udp/0/quic-v1").await;
    let (listener_connection, dialer_connection) = connect(&mut listener, &mut dialer, addr).await;

    assert_eq!(
        dialer_builds.load(Ordering::SeqCst),
        1,
        "the dialing transport must build its controller through the configured factory"
    );
    assert_eq!(
        listener_builds.load(Ordering::SeqCst),
        1,
        "the listening transport must build its controller through the configured factory"
    );

    round_trip(dialer_connection, listener_connection).await;
}

#[tokio::test]
async fn default_transport_still_works() {
    let (_, mut listener) = create_transport(|config| config);
    let (_, mut dialer) = create_transport(|config| config);

    let addr = start_listening(&mut listener, "/ip4/127.0.0.1/udp/0/quic-v1").await;
    let (listener_connection, dialer_connection) = connect(&mut listener, &mut dialer, addr).await;

    round_trip(dialer_connection, listener_connection).await;
}
