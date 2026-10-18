//! BBRv2 congestion control for QUIC.
//!
//! This module provides an implementation of the BBRv2 congestion control
//! algorithm, ported from Cloudflare's [quiche] implementation, and wired into
//! Quinn's [`congestion::Controller`](quinn::congestion::Controller) interface.
//!
//! BBRv2 targets higher throughput than loss-based congestion controllers such
//! as the default CUBIC on paths with a high bandwidth-delay product or with
//! shallow buffers, while keeping the queueing delay in check. It is opt-in: see
//! [`Config::with_bbr2`](crate::Config::with_bbr2) and
//! [`Config::with_bbr2_params`](crate::Config::with_bbr2_params). A QUIC
//! connection upgrades to BBRv2 only if both peers run this build; otherwise
//! both ends fall back to the controller they were configured with.
//!
//! The implementation is split into two layers:
//!
//! * [`core`] is a standalone port of the algorithm that is free of any Quinn
//!   types. It tracks delivery rate samples, the network model, and the
//!   `Startup`, `Drain`, `ProbeBW` and `ProbeRTT` state machine. All time is
//!   passed in explicitly rather than read from the clock, which makes the
//!   behaviour reproducible in tests.
//! * [`Bbr2`] adapts the core state machine to Quinn's per-packet event stream
//!   (`on_sent`, `on_ack`, `on_end_acks`, `on_congestion_event`), and
//!   [`Bbr2Config`] implements Quinn's congestion controller factory so that a
//!   [`TransportConfig`](quinn::TransportConfig) can build one per connection.
//!
//! [quiche]: https://github.com/cloudflare/quiche/blob/master/quiche/src/recovery/gcongestion/bbr2.rs

pub mod core;

mod controller;

pub use self::core::{Bandwidth, Bbr2Params, BbrBwLoReductionStrategy};
pub use controller::{Bbr2, Bbr2Config};
