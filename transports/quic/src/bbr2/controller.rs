use std::{
    any::Any,
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use quinn_proto::{
    congestion::{Controller, ControllerFactory},
    RttEstimator,
};

use crate::bbr2::core::{
    bandwidth_sampler::{Acked, Lost},
    bbr2::{BBRv2, Bbr2Params},
    Bandwidth,
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

    /// Returns the BBRv2 model's computed pacing rate.
    ///
    /// Note that `quinn-proto 0.11` drives pacing internally from the
    /// congestion window and smoothed RTT.
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
        self.bytes_in_flight = bytes_in_flight + bytes;
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
        let total_size = bytes as usize;
        if total_size == 0 {
            return;
        }

        let mss = (self.current_mtu as usize).max(BASE_DATAGRAM_SIZE as usize);
        let mut remaining = total_size;

        while remaining > 0 {
            let chunk_size = remaining.min(mss);
            let prior_in_flight = self.bytes_in_flight;
            self.bytes_in_flight += chunk_size;
            let packet_number = self.next_packet_number;
            self.next_packet_number += 1;

            self.sent_packets.push_back(SentPacketRecord {
                packet_number,
                sent_time: now,
                size: chunk_size,
                acked: false,
                lost: false,
            });

            self.core
                .on_packet_sent(now, prior_in_flight, packet_number, chunk_size, true);

            remaining -= chunk_size;
        }
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
        if size == 0 {
            return;
        }

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
            let mut remaining = size;
            for record in &mut self.sent_packets {
                if !record.acked && !record.lost && record.sent_time == sent {
                    if record.size <= remaining {
                        record.acked = true;
                        remaining -= record.size;
                        if matched_packet.is_none() {
                            matched_packet = Some(record.packet_number);
                        }
                        if remaining == 0 {
                            break;
                        }
                    } else {
                        record.size -= remaining;
                        self.core
                            .record_partial_ack(record.packet_number, remaining);
                        if matched_packet.is_none() {
                            matched_packet = Some(record.packet_number);
                        }
                        break;
                    }
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

        // Reconcile silently discarded packets (e.g. Initial/Handshake keys discarded by Quinn)
        let tracked_in_flight: usize = self
            .sent_packets
            .iter()
            .filter(|r| !r.acked && !r.lost)
            .map(|r| r.size)
            .sum();

        let mut excess = tracked_in_flight.saturating_sub(self.bytes_in_flight);
        if excess > 0 {
            for record in &mut self.sent_packets {
                if !record.acked && !record.lost {
                    if record.size <= excess {
                        record.acked = true;
                        excess -= record.size;
                        self.core.on_packet_neutered(record.packet_number);
                        if excess == 0 {
                            break;
                        }
                    } else {
                        record.size -= excess;
                        self.core
                            .on_packet_partially_neutered(record.packet_number, excess);
                        break;
                    }
                }
            }
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
        if self.sent_packets.is_empty() {
            self.least_unacked_packet = self.next_packet_number;
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
        now: Instant,
        sent: Instant,
        _is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        if lost_bytes == 0 {
            return;
        }

        let size = lost_bytes as usize;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(size);

        // Quinn reports aggregate lost bytes across all lost packets in the batch,
        // and `sent` is the send time of the largest/newest lost packet in that batch.
        // Earlier lost packets have `sent_time <= sent`.
        let mut remaining = size;
        let mut any_matched = false;

        for record in self.sent_packets.iter_mut().rev() {
            if !record.acked && !record.lost && record.sent_time == sent {
                if record.size <= remaining {
                    record.lost = true;
                    remaining -= record.size;
                    self.pending_losses.push(Lost {
                        packet_number: record.packet_number,
                        bytes_lost: record.size,
                    });
                    any_matched = true;
                    if remaining == 0 {
                        break;
                    }
                } else {
                    record.size -= remaining;
                    self.pending_losses.push(Lost {
                        packet_number: record.packet_number,
                        bytes_lost: remaining,
                    });
                    remaining = 0;
                    any_matched = true;
                    break;
                }
            }
        }

        if remaining > 0 {
            for record in self.sent_packets.iter_mut().rev() {
                if !record.acked && !record.lost && record.sent_time < sent {
                    if record.size <= remaining {
                        record.lost = true;
                        remaining -= record.size;
                        self.pending_losses.push(Lost {
                            packet_number: record.packet_number,
                            bytes_lost: record.size,
                        });
                        any_matched = true;
                        if remaining == 0 {
                            break;
                        }
                    } else {
                        record.size -= remaining;
                        self.pending_losses.push(Lost {
                            packet_number: record.packet_number,
                            bytes_lost: remaining,
                        });
                        any_matched = true;
                        break;
                    }
                }
            }
        }

        if !any_matched {
            self.pending_losses.push(Lost {
                packet_number: self.least_unacked_packet,
                bytes_lost: size,
            });
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
        if self.sent_packets.is_empty() {
            self.least_unacked_packet = self.next_packet_number;
        }

        if self.pending_acks.is_empty() && !self.pending_losses.is_empty() {
            self.pending_losses.sort_by_key(|l| l.packet_number);
            let total_lost: usize = self.pending_losses.iter().map(|l| l.bytes_lost).sum();
            let prior_in_flight = self.bytes_in_flight + total_lost;

            self.core.on_congestion_event(
                prior_in_flight,
                now,
                &[],
                &self.pending_losses,
                self.least_unacked_packet,
            );

            self.pending_losses.clear();
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu;
        self.core.update_mss(new_mtu as usize);
    }

    fn window(&self) -> u64 {
        self.core.get_congestion_window() as u64
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

    fn dummy_rtt() -> Box<RttEstimator> {
        // RttEstimator does not expose a public constructor, but is unused by Bbr2::on_ack.
        unsafe { Box::new(std::mem::zeroed::<RttEstimator>()) }
    }

    #[test]
    fn factory_builds_a_controller_with_the_configured_initial_window() {
        let mut config = Bbr2Config::default();
        config.initial_window(20 * MTU as u64);

        let controller = Arc::new(config).build(Instant::now(), MTU);

        assert_eq!(controller.initial_window(), 20 * MTU as u64);
        assert_eq!(controller.window(), 20 * MTU as u64);
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

    #[test]
    fn pre_send_in_flight_exits_quiescence_on_new_send() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);
        let rtt = dummy_rtt();

        // Send 1 packet
        controller.on_sent(now, MTU as u64, 1);
        // Ack 1 packet with 0 in-flight -> enters quiescence
        controller.on_ack(
            now + Duration::from_millis(50),
            now,
            MTU as u64,
            false,
            &rtt,
        );
        controller.on_end_acks(now + Duration::from_millis(50), 0, false, Some(1));

        // Should be in quiescence (is_in_recovery is false when in quiescence)
        assert!(!controller.core().is_in_recovery());

        // Send new packet after idling
        let later = now + Duration::from_secs(1);
        controller.on_sent(later, MTU as u64, 2);

        // Should exit quiescence (is_in_recovery becomes true)
        assert!(controller.core().is_in_recovery());
    }

    #[test]
    fn batched_gso_send_accounting() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);
        let rtt = dummy_rtt();

        // GSO batch of 2400 bytes (2 * MTU)
        controller.on_sent(now, 2 * MTU as u64, 2);

        // 50ms later, only 1200 bytes is ACKed
        controller.on_ack(
            now + Duration::from_millis(50),
            now,
            MTU as u64,
            false,
            &rtt,
        );
        controller.on_end_acks(now + Duration::from_millis(50), MTU as u64, false, Some(1));

        // Window must grow by at most the acked bytes (1200), NOT by the full 2400 batch!
        let window_after_first_ack = controller.window();
        assert_eq!(window_after_first_ack, 10 * MTU as u64 + MTU as u64);
        assert_eq!(controller.sent_packets.len(), 1);

        // Ack the second 1200 bytes
        controller.on_ack(
            now + Duration::from_millis(50),
            now,
            MTU as u64,
            false,
            &rtt,
        );
        controller.on_end_acks(now + Duration::from_millis(50), 0, false, Some(2));

        assert_eq!(controller.window(), 10 * MTU as u64 + 2 * MTU as u64);
        assert_eq!(controller.sent_packets.len(), 0);
    }

    #[test]
    fn retire_all_packets_in_loss_batch() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);

        // Send 3 separate packets at distinct timestamps
        let t1 = now;
        controller.on_sent(t1, MTU as u64, 1);
        let t2 = now + Duration::from_millis(10);
        controller.on_sent(t2, MTU as u64, 2);
        let t3 = now + Duration::from_millis(20);
        controller.on_sent(t3, MTU as u64, 3);

        assert_eq!(controller.sent_packets.len(), 3);

        // Quinn reports 3600-byte loss batch with the newest packet's timestamp (t3)
        controller.on_congestion_event(now + Duration::from_millis(50), t3, false, 3 * MTU as u64);
        controller.on_end_acks(now + Duration::from_millis(50), 0, false, None);

        // All 3 packets must be retired!
        assert!(
            controller.sent_packets.is_empty(),
            "sent_packets should be empty after all are lost"
        );
        assert_eq!(controller.least_unacked_packet, 4);
    }

    #[test]
    fn clean_up_discarded_handshake_packets() {
        let now = Instant::now();
        let mut controller = Bbr2::new(Arc::new(Bbr2Config::default()), now, MTU);
        let rtt = dummy_rtt();

        // Initial space packets sent during handshake
        let t1 = now;
        controller.on_sent(t1, MTU as u64, 1);
        controller.on_sent(t1, MTU as u64, 2);

        // Data space packets sent
        let t2 = now + Duration::from_millis(10);
        controller.on_sent(t2, MTU as u64, 3);
        controller.on_sent(t2, MTU as u64, 4);

        assert_eq!(controller.sent_packets.len(), 4);

        // Handshake keys dropped by Quinn -> Initial packets discarded without on_ack/on_loss.
        // Data packets ACKed. in_flight drops to 0.
        controller.on_ack(now + Duration::from_millis(50), t2, MTU as u64, false, &rtt);
        controller.on_ack(now + Duration::from_millis(50), t2, MTU as u64, false, &rtt);
        controller.on_end_acks(now + Duration::from_millis(50), 0, false, Some(4));

        // The discarded Initial packets must NOT remain in sent_packets!
        assert!(
            controller.sent_packets.is_empty(),
            "sent_packets must not leak discarded Initial packets"
        );
        assert_eq!(controller.least_unacked_packet, 5);
    }
}
