use std::{
    any::Any,
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use quinn_proto::{
    RttEstimator,
    congestion::{Controller, ControllerFactory, ControllerMetrics},
};

use crate::bbr2::core::{
    Bandwidth,
    bandwidth_sampler::{Acked, Lost},
    bbr2::{BBRv2, Bbr2Params},
};

const BASE_DATAGRAM_SIZE: u64 = 1200;
const DEFAULT_INITIAL_WINDOW_PACKETS: usize = 10;
const DEFAULT_MAX_WINDOW_PACKETS: usize = 10_000;
const DEFAULT_INITIAL_RTT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
struct SentPacketRecord {
    packet_number: u64,
    sent_time: Instant,
    size: usize,
    acked: bool,
    lost: bool,
}

/// BBRv2 congestion controller implementation for Quinn.
#[derive(Debug, Clone)]
pub struct Bbr2 {
    core: BBRv2,
    config: Arc<Bbr2Config>,
    current_mtu: u16,
    bytes_in_flight: usize,
    sent_packets: VecDeque<SentPacketRecord>,
    pending_acks: Vec<Acked>,
    pending_losses: Vec<Lost>,
    least_unacked_packet: u64,
    next_packet_number: u64,
    pending_acked_bytes: usize,
}

impl Bbr2 {
    /// Construct a new BBRv2 controller using the provided configuration, initial time, and MTU.
    pub fn new(config: Arc<Bbr2Config>, now: Instant, current_mtu: u16) -> Self {
        let mtu = if current_mtu == 0 {
            BASE_DATAGRAM_SIZE as usize
        } else {
            current_mtu as usize
        };

        let initial_packets = ((config.initial_window / mtu as u64).max(2)) as usize;
        let max_packets = ((config.max_window / mtu as u64).max(initial_packets as u64)) as usize;

        let core = BBRv2::new(
            initial_packets,
            max_packets,
            mtu,
            config.initial_rtt,
            now,
            config.params.as_ref(),
        );

        Bbr2 {
            core,
            config,
            current_mtu: mtu as u16,
            bytes_in_flight: 0,
            sent_packets: VecDeque::new(),
            pending_acks: Vec::new(),
            pending_losses: Vec::new(),
            least_unacked_packet: 0,
            next_packet_number: 1,
            pending_acked_bytes: 0,
        }
    }

    /// Access the underlying BBRv2 core state machine.
    pub fn core(&self) -> &BBRv2 {
        &self.core
    }

    /// Access the underlying BBRv2 core state machine mutably.
    pub fn core_mut(&mut self) -> &mut BBRv2 {
        &mut self.core
    }

    /// Returns the current pacing rate.
    pub fn pacing_rate(&self) -> Bandwidth {
        self.core.pacing_rate()
    }

    /// Returns the estimated bandwidth.
    pub fn bandwidth_estimate(&self) -> Bandwidth {
        self.core.bandwidth_estimate()
    }

    /// Returns the maximum bandwidth observed.
    pub fn max_bandwidth(&self) -> Bandwidth {
        self.core.max_bandwidth()
    }

    /// Returns the minimum RTT observed.
    pub fn min_rtt(&self) -> Duration {
        self.core.min_rtt()
    }

    /// Direct event handler for sending packets with authoritative packet information.
    pub fn on_packet_sent_direct(
        &mut self,
        sent_time: Instant,
        bytes_in_flight: usize,
        packet_number: u64,
        bytes: usize,
        is_retransmissible: bool,
    ) {
        self.bytes_in_flight = bytes_in_flight;
        self.core.on_packet_sent(
            sent_time,
            bytes_in_flight,
            packet_number,
            bytes,
            is_retransmissible,
        );
    }

    /// Direct event handler for processing complete congestion event batches.
    pub fn on_congestion_event_direct(
        &mut self,
        prior_in_flight: usize,
        bytes_in_flight: usize,
        event_time: Instant,
        acked: &[Acked],
        lost: &[Lost],
        least_unacked: u64,
    ) {
        self.bytes_in_flight = bytes_in_flight;
        self.core
            .on_congestion_event(prior_in_flight, event_time, acked, lost, least_unacked);
    }
}

impl Controller for Bbr2 {
    fn on_sent(&mut self, now: Instant, bytes: u64, _last_packet_number: u64) {
        let size = bytes as usize;
        self.bytes_in_flight += size;
        let packet_number = self.next_packet_number;
        self.next_packet_number += 1;

        self.sent_packets.push_back(SentPacketRecord {
            packet_number,
            sent_time: now,
            size,
            acked: false,
            lost: false,
        });

        self.core
            .on_packet_sent(now, self.bytes_in_flight, packet_number, size, true);
    }

    fn on_ack(
        &mut self,
        _now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        _rtt: &RttEstimator,
    ) {
        if app_limited {
            self.core.on_app_limited();
        }

        let size = bytes as usize;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(size);
        self.pending_acked_bytes += size;

        let mut matched_packet = None;
        for record in &mut self.sent_packets {
            if !record.acked && !record.lost && record.sent_time == sent && record.size == size {
                record.acked = true;
                matched_packet = Some(record.packet_number);
                break;
            }
        }

        if matched_packet.is_none() {
            for record in &mut self.sent_packets {
                if !record.acked && !record.lost && record.sent_time == sent {
                    record.acked = true;
                    matched_packet = Some(record.packet_number);
                    break;
                }
            }
        }

        let pkt_num = matched_packet.unwrap_or(self.least_unacked_packet);
        self.pending_acks.push(Acked {
            pkt_num,
            time_sent: sent,
        });
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        _largest_packet_num_acked: Option<u64>,
    ) {
        self.bytes_in_flight = in_flight as usize;

        if app_limited {
            self.core.on_app_limited();
        }

        while let Some(front) = self.sent_packets.front() {
            if front.acked || front.lost {
                self.least_unacked_packet = front.packet_number + 1;
                self.sent_packets.pop_front();
            } else {
                self.least_unacked_packet = front.packet_number;
                break;
            }
        }

        if !self.pending_acks.is_empty() || !self.pending_losses.is_empty() {
            self.pending_acks.sort_by_key(|a| a.pkt_num);
            self.pending_losses.sort_by_key(|l| l.packet_number);

            let total_acked = self.pending_acked_bytes;
            let total_lost: usize = self.pending_losses.iter().map(|l| l.bytes_lost).sum();
            let prior_in_flight = self.bytes_in_flight + total_acked + total_lost;

            self.core.on_congestion_event(
                prior_in_flight,
                now,
                &self.pending_acks,
                &self.pending_losses,
                self.least_unacked_packet,
            );

            self.pending_acks.clear();
            self.pending_losses.clear();
            self.pending_acked_bytes = 0;
        }
    }

    fn on_congestion_event(
        &mut self,
        _now: Instant,
        sent: Instant,
        _is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        if lost_bytes == 0 {
            return;
        }

        let size = lost_bytes as usize;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(size);

        let mut matched_packet = None;
        for record in &mut self.sent_packets {
            if !record.acked && !record.lost && record.sent_time == sent {
                record.lost = true;
                matched_packet = Some(record.packet_number);
                break;
            }
        }

        let packet_number = matched_packet.unwrap_or(self.least_unacked_packet);
        self.pending_losses.push(Lost {
            packet_number,
            bytes_lost: size,
        });
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu;
        self.core.update_mss(new_mtu as usize);
    }

    fn window(&self) -> u64 {
        self.core.get_congestion_window() as u64
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = ControllerMetrics::default();
        metrics.congestion_window = self.window();
        metrics.pacing_rate = Some(self.core.pacing_rate().to_bits_per_second());
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Configuration for the [`Bbr2`] congestion controller.
#[derive(Debug, Clone)]
pub struct Bbr2Config {
    pub(crate) initial_window: u64,
    pub(crate) max_window: u64,
    pub(crate) initial_rtt: Duration,
    pub(crate) params: Option<Bbr2Params>,
}

impl Bbr2Config {
    /// Create a new BBRv2 configuration with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new BBRv2 configuration with specific parameters.
    pub fn from_params(params: Bbr2Params) -> Self {
        Self {
            params: Some(params),
            ..Self::default()
        }
    }

    /// Set custom BBRv2 algorithm parameters.
    pub fn with_params(mut self, params: Bbr2Params) -> Self {
        self.params = Some(params);
        self
    }

    /// Set the initial congestion window in bytes.
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }

    /// Set the maximum congestion window in bytes.
    pub fn max_window(&mut self, value: u64) -> &mut Self {
        self.max_window = value;
        self
    }

    /// Set the assumed initial round-trip time.
    pub fn initial_rtt(&mut self, value: Duration) -> &mut Self {
        self.initial_rtt = value;
        self
    }

    /// Set customized BBRv2 algorithm parameters.
    pub fn params(&mut self, params: Bbr2Params) -> &mut Self {
        self.params = Some(params);
        self
    }
}

impl Default for Bbr2Config {
    fn default() -> Self {
        Self {
            initial_window: DEFAULT_INITIAL_WINDOW_PACKETS as u64 * BASE_DATAGRAM_SIZE,
            max_window: DEFAULT_MAX_WINDOW_PACKETS as u64 * BASE_DATAGRAM_SIZE,
            initial_rtt: DEFAULT_INITIAL_RTT,
            params: None,
        }
    }
}

impl ControllerFactory for Bbr2Config {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Bbr2::new(self, now, current_mtu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: u16 = 1200;

    #[test]
    fn factory_builds_a_controller_with_the_configured_initial_window() {
        let mut config = Bbr2Config::default();
        config.initial_window(20 * MTU as u64);

        let controller = Arc::new(config).build(Instant::now(), MTU);

        assert_eq!(controller.initial_window(), 20 * MTU as u64);
        assert_eq!(controller.window(), 20 * MTU as u64);
        // BBRv2 does not use a slow start threshold, but it does pace.
        let metrics = controller.metrics();
        assert_eq!(metrics.congestion_window, 20 * MTU as u64);
        assert_eq!(metrics.ssthresh, None);
        assert!(metrics.pacing_rate.is_some_and(|rate| rate > 0));
    }

    #[test]
    fn controller_is_cloneable_and_downcastable() {
        let controller = Arc::new(Bbr2Config::default()).build(Instant::now(), MTU);
        let window = controller.window();

        assert_eq!(controller.clone_box().window(), window);
        assert!(controller.into_any().downcast::<Bbr2>().is_ok());
    }

    #[test]
    fn delivery_events_update_the_network_model() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);

        assert_eq!(controller.core().state_str(), "bbr_startup");
        assert_eq!(controller.core().get_congestion_window(), 10 * MTU as usize);

        // Sending alone must not change the window.
        let mut packet_number = 0;
        let mut bytes_in_flight = 0;
        let mut acked = Vec::new();
        for _ in 0..10 {
            packet_number += 1;
            bytes_in_flight += MTU as usize;
            controller.on_packet_sent_direct(
                now,
                bytes_in_flight,
                packet_number,
                MTU as usize,
                true,
            );
            acked.push(Acked {
                pkt_num: packet_number,
                time_sent: now,
            });
        }
        assert_eq!(controller.core().get_congestion_window(), 10 * MTU as usize);

        controller.on_congestion_event_direct(
            bytes_in_flight,
            0,
            now + Duration::from_millis(50),
            &acked,
            &[],
            packet_number,
        );

        assert!(controller.core().bandwidth_estimate() > Bandwidth::zero());
        assert!(controller.window() > 0);
    }

    #[test]
    fn mtu_updates_scale_the_window() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);
        let window_before = controller.window();

        controller.on_mtu_update(1400);

        assert_eq!(controller.window(), window_before * 1400 / MTU as u64);
    }
}
