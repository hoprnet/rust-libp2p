// Copyright (c) 2015 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Copyright (C) 2023, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::time::{Duration, Instant};

use super::{
    bandwidth::Bandwidth,
    bandwidth_sampler::{Acked, Lost, SendTimeState},
    mode::Mode,
    network_model::BBRv2NetworkModel,
};

const MAX_MODE_CHANGES_PER_CONGESTION_EVENT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BbrBwLoReductionStrategy {
    Default = 0,
    MinRttReduction = 1,
    InflightReduction = 2,
    CwndReduction = 3,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum BwLoMode {
    Default,
    MinRttReduction,
    InflightReduction,
    CwndReduction,
}

impl From<BbrBwLoReductionStrategy> for BwLoMode {
    fn from(value: BbrBwLoReductionStrategy) -> Self {
        match value {
            BbrBwLoReductionStrategy::Default => BwLoMode::Default,
            BbrBwLoReductionStrategy::MinRttReduction => BwLoMode::MinRttReduction,
            BbrBwLoReductionStrategy::InflightReduction => BwLoMode::InflightReduction,
            BbrBwLoReductionStrategy::CwndReduction => BwLoMode::CwndReduction,
        }
    }
}

/// Customizable parameters for BBRv2.
///
/// Every field is optional: `None` (the default) selects the algorithm's
/// built-in value for that parameter, so `Bbr2Params::default()` matches
/// the upstream defaults.
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct Bbr2Params {
    pub startup_cwnd_gain: Option<f32>,
    pub startup_pacing_gain: Option<f32>,
    pub full_bw_threshold: Option<f32>,
    pub startup_full_bw_rounds: Option<usize>,
    pub startup_full_loss_count: Option<usize>,
    pub drain_cwnd_gain: Option<f32>,
    pub drain_pacing_gain: Option<f32>,
    pub enable_reno_coexistence: Option<bool>,
    pub enable_overestimate_avoidance: Option<bool>,
    pub choose_a0_point_fix: Option<bool>,
    pub probe_bw_probe_up_pacing_gain: Option<f32>,
    pub probe_bw_probe_down_pacing_gain: Option<f32>,
    pub probe_bw_cwnd_gain: Option<f32>,
    pub probe_bw_up_cwnd_gain: Option<f32>,
    pub probe_rtt_pacing_gain: Option<f32>,
    pub probe_rtt_cwnd_gain: Option<f32>,
    pub probe_rtt_duration: Option<Duration>,
    pub probe_rtt_period: Option<Duration>,
    pub max_probe_up_queue_rounds: Option<usize>,
    pub loss_threshold: Option<f32>,
    pub use_bytes_delivered_for_inflight_hi: Option<bool>,
    pub decrease_startup_pacing_at_end_of_round: Option<bool>,
    pub bw_lo_reduction_strategy: Option<BbrBwLoReductionStrategy>,
    pub ignore_app_limited_for_no_bandwidth_growth: Option<bool>,
    pub initial_pacing_rate_bytes_per_second: Option<u64>,
    pub min_cwnd_packets: Option<usize>,
    pub scale_pacing_rate_by_mss: Option<bool>,
    pub disable_probe_down_early_exit: Option<bool>,
    pub time_sent_set_to_now: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct Params {
    pub(crate) startup_cwnd_gain: f32,
    pub(crate) startup_pacing_gain: f32,
    pub(crate) full_bw_threshold: f32,
    pub(crate) startup_full_bw_rounds: usize,
    pub(crate) max_startup_queue_rounds: usize,
    pub(crate) startup_full_loss_count: usize,
    pub(crate) drain_cwnd_gain: f32,
    pub(crate) drain_pacing_gain: f32,
    pub(crate) probe_bw_probe_max_rounds: usize,
    pub(crate) enable_reno_coexistence: bool,
    pub(crate) probe_bw_probe_reno_gain: f32,
    pub(crate) probe_bw_probe_base_duration: Duration,
    pub(crate) probe_bw_full_loss_count: usize,
    pub(crate) probe_bw_probe_up_pacing_gain: f32,
    pub(crate) probe_bw_probe_down_pacing_gain: f32,
    pub(crate) probe_bw_default_pacing_gain: f32,
    pub(crate) probe_bw_cwnd_gain: f32,
    pub(crate) probe_bw_up_cwnd_gain: f32,
    pub(crate) probe_up_ignore_inflight_hi: bool,
    pub(crate) max_probe_up_queue_rounds: usize,
    pub(crate) probe_rtt_inflight_target_bdp_fraction: f32,
    pub(crate) probe_rtt_period: Duration,
    pub(crate) probe_rtt_duration: Duration,
    pub(crate) probe_rtt_pacing_gain: f32,
    pub(crate) probe_rtt_cwnd_gain: f32,
    pub(crate) initial_max_ack_height_filter_window: usize,
    pub(crate) inflight_hi_headroom: f32,
    pub(crate) loss_threshold: f32,
    pub(crate) beta: f32,
    pub(crate) add_ack_height_to_queueing_threshold: bool,
    pub(crate) avoid_unnecessary_probe_rtt: bool,
    pub(crate) limit_inflight_hi_by_max_delivered: bool,
    pub(crate) startup_loss_exit_use_max_delivered_for_inflight_hi: bool,
    pub(crate) use_bytes_delivered_for_inflight_hi: bool,
    pub(crate) decrease_startup_pacing_at_end_of_round: bool,
    pub(crate) enable_overestimate_avoidance: bool,
    pub(crate) choose_a0_point_fix: bool,
    pub(crate) bw_lo_mode: BwLoMode,
    pub(crate) ignore_app_limited_for_no_bandwidth_growth: bool,
    pub(crate) initial_pacing_rate_bytes_per_second: Option<u64>,
    pub(crate) min_cwnd_packets: Option<usize>,
    pub(crate) scale_pacing_rate_by_mss: bool,
    pub(crate) disable_probe_down_early_exit: bool,
    pub(crate) time_sent_set_to_now: bool,
}

pub(crate) const DEFAULT_PARAMS: Params = Params {
    startup_cwnd_gain: 2.0,
    startup_pacing_gain: 2.773,
    full_bw_threshold: 1.25,
    startup_full_bw_rounds: 3,
    max_startup_queue_rounds: 0,
    startup_full_loss_count: 8,
    drain_cwnd_gain: 2.0,
    drain_pacing_gain: 1.0 / 2.885,
    probe_bw_probe_max_rounds: 63,
    enable_reno_coexistence: true,
    probe_bw_probe_reno_gain: 1.0,
    probe_bw_probe_base_duration: Duration::from_millis(2000),
    probe_bw_full_loss_count: 2,
    probe_bw_probe_up_pacing_gain: 1.25,
    probe_bw_probe_down_pacing_gain: 0.9,
    probe_bw_default_pacing_gain: 1.0,
    probe_bw_cwnd_gain: 2.0,
    probe_bw_up_cwnd_gain: 2.25,
    probe_up_ignore_inflight_hi: false,
    max_probe_up_queue_rounds: 2,
    probe_rtt_inflight_target_bdp_fraction: 0.5,
    probe_rtt_period: Duration::from_millis(10000),
    probe_rtt_duration: Duration::from_millis(200),
    probe_rtt_pacing_gain: 1.0,
    probe_rtt_cwnd_gain: 1.0,
    initial_max_ack_height_filter_window: 10,
    inflight_hi_headroom: 0.15,
    loss_threshold: 0.015,
    beta: 0.3,
    add_ack_height_to_queueing_threshold: false,
    avoid_unnecessary_probe_rtt: true,
    limit_inflight_hi_by_max_delivered: true,
    startup_loss_exit_use_max_delivered_for_inflight_hi: true,
    use_bytes_delivered_for_inflight_hi: true,
    decrease_startup_pacing_at_end_of_round: true,
    enable_overestimate_avoidance: false,
    choose_a0_point_fix: false,
    bw_lo_mode: BwLoMode::Default,
    ignore_app_limited_for_no_bandwidth_growth: true,
    initial_pacing_rate_bytes_per_second: None,
    min_cwnd_packets: None,
    scale_pacing_rate_by_mss: false,
    disable_probe_down_early_exit: false,
    time_sent_set_to_now: true,
};

impl Params {
    fn with_overrides(mut self, custom: &Bbr2Params) -> Self {
        macro_rules! apply_override {
            ($field:ident) => {
                if let Some(val) = custom.$field {
                    self.$field = val;
                }
            };
        }

        apply_override!(startup_cwnd_gain);
        apply_override!(startup_pacing_gain);
        apply_override!(full_bw_threshold);
        apply_override!(startup_full_bw_rounds);
        apply_override!(startup_full_loss_count);
        apply_override!(drain_cwnd_gain);
        apply_override!(drain_pacing_gain);
        apply_override!(enable_reno_coexistence);
        apply_override!(enable_overestimate_avoidance);
        apply_override!(choose_a0_point_fix);
        apply_override!(probe_bw_probe_up_pacing_gain);
        apply_override!(probe_bw_probe_down_pacing_gain);
        apply_override!(probe_bw_cwnd_gain);
        apply_override!(probe_bw_up_cwnd_gain);
        apply_override!(probe_rtt_pacing_gain);
        apply_override!(probe_rtt_cwnd_gain);
        apply_override!(probe_rtt_duration);
        apply_override!(probe_rtt_period);
        apply_override!(max_probe_up_queue_rounds);
        apply_override!(loss_threshold);
        apply_override!(use_bytes_delivered_for_inflight_hi);
        apply_override!(decrease_startup_pacing_at_end_of_round);
        apply_override!(ignore_app_limited_for_no_bandwidth_growth);
        apply_override!(scale_pacing_rate_by_mss);
        apply_override!(disable_probe_down_early_exit);
        apply_override!(time_sent_set_to_now);

        if let Some(val) = custom.initial_pacing_rate_bytes_per_second {
            self.initial_pacing_rate_bytes_per_second = Some(val);
        }
        if let Some(val) = custom.min_cwnd_packets {
            self.min_cwnd_packets = Some(val);
        }
        if let Some(val) = custom.bw_lo_reduction_strategy {
            self.bw_lo_mode = val.into();
        }

        self
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits<T: Ord> {
    pub(crate) lo: T,
    pub(crate) hi: T,
}

impl<T: Ord + Clone + Copy> Limits<T> {
    pub(crate) fn min(&self) -> T {
        self.lo
    }

    pub(crate) fn apply_limits(&self, val: T) -> T {
        val.max(self.lo).min(self.hi)
    }
}

impl<T: Ord + Clone + Copy + From<u8>> Limits<T> {
    pub(crate) fn no_greater_than(val: T) -> Self {
        Self {
            lo: T::from(0),
            hi: val,
        }
    }
}

fn initial_pacing_rate(cwnd_in_bytes: usize, smoothed_rtt: Duration, params: &Params) -> Bandwidth {
    if let Some(pacing_rate) = params.initial_pacing_rate_bytes_per_second {
        return Bandwidth::from_bytes_per_second(pacing_rate);
    }

    Bandwidth::from_bytes_and_time_delta(cwnd_in_bytes, smoothed_rtt) * 2.885
}

#[derive(Debug)]
pub(crate) struct BBRv2CongestionEvent {
    pub(crate) event_time: Instant,
    pub(crate) prior_cwnd: usize,
    pub(crate) prior_bytes_in_flight: usize,
    pub(crate) bytes_in_flight: usize,
    pub(crate) bytes_acked: usize,
    pub(crate) bytes_lost: usize,
    pub(crate) end_of_round_trip: bool,
    pub(crate) is_probing_for_bandwidth: bool,
    pub(crate) sample_max_bandwidth: Option<Bandwidth>,
    pub(crate) sample_min_rtt: Option<Duration>,
    pub(crate) last_packet_send_state: SendTimeState,
}

impl BBRv2CongestionEvent {
    pub(crate) fn new(
        event_time: Instant,
        prior_cwnd: usize,
        prior_bytes_in_flight: usize,
        is_probing_for_bandwidth: bool,
    ) -> Self {
        BBRv2CongestionEvent {
            event_time,
            prior_cwnd,
            prior_bytes_in_flight,
            is_probing_for_bandwidth,
            bytes_in_flight: 0,
            bytes_acked: 0,
            bytes_lost: 0,
            end_of_round_trip: false,
            last_packet_send_state: Default::default(),
            sample_max_bandwidth: None,
            sample_min_rtt: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BBRv2 {
    mode: Mode,
    cwnd: usize,
    mss: usize,
    pacing_rate: Bandwidth,
    cwnd_limits: Limits<usize>,
    initial_cwnd: usize,
    pub(crate) last_sample_is_app_limited: bool,
    pub(crate) has_non_app_limited_sample: bool,
    last_quiescence_start: Option<Instant>,
    params: Params,
}

impl BBRv2 {
    pub fn new(
        initial_congestion_window: usize,
        max_congestion_window: usize,
        max_segment_size: usize,
        smoothed_rtt: Duration,
        now: Instant,
        custom_bbr_params: Option<&Bbr2Params>,
    ) -> Self {
        let cwnd = initial_congestion_window * max_segment_size;

        let params = if let Some(custom) = custom_bbr_params {
            DEFAULT_PARAMS.with_overrides(custom)
        } else {
            DEFAULT_PARAMS
        };

        let min_cwnd =
            params.min_cwnd_packets.unwrap_or(initial_congestion_window) * max_segment_size;

        BBRv2 {
            mode: Mode::startup(BBRv2NetworkModel::new(&params, smoothed_rtt, now)),
            cwnd,
            pacing_rate: initial_pacing_rate(cwnd, smoothed_rtt, &params),
            cwnd_limits: Limits {
                lo: min_cwnd,
                hi: max_congestion_window * max_segment_size,
            },
            initial_cwnd: cwnd,
            last_sample_is_app_limited: false,
            has_non_app_limited_sample: false,
            last_quiescence_start: None,
            mss: max_segment_size,
            params,
        }
    }

    pub fn get_congestion_window(&self) -> usize {
        self.cwnd
    }

    pub fn get_congestion_window_in_packets(&self) -> usize {
        self.cwnd / self.mss
    }

    pub fn can_send(&self, bytes_in_flight: usize) -> bool {
        bytes_in_flight < self.get_congestion_window()
    }

    pub fn pacing_rate(&self) -> Bandwidth {
        self.pacing_rate
    }

    pub fn bandwidth_estimate(&self) -> Bandwidth {
        self.mode.network_model().bandwidth_estimate()
    }

    pub fn max_bandwidth(&self) -> Bandwidth {
        self.mode.network_model().max_bandwidth()
    }

    pub fn min_rtt(&self) -> Duration {
        self.mode.network_model().min_rtt()
    }

    pub fn state_str(&self) -> &'static str {
        self.mode.state_str()
    }

    pub fn is_in_recovery(&self) -> bool {
        self.last_quiescence_start.is_none()
    }

    pub fn is_cwnd_limited(&self, bytes_in_flight: usize) -> bool {
        bytes_in_flight >= self.get_congestion_window()
    }

    pub fn update_mss(&mut self, new_mss: usize) {
        if self.mss == 0 || new_mss == 0 {
            return;
        }
        self.cwnd_limits.hi =
            (self.cwnd_limits.hi as u64 * new_mss as u64 / self.mss as u64) as usize;
        self.cwnd_limits.lo =
            (self.cwnd_limits.lo as u64 * new_mss as u64 / self.mss as u64) as usize;
        self.cwnd = (self.cwnd as u64 * new_mss as u64 / self.mss as u64) as usize;
        self.initial_cwnd = (self.initial_cwnd as u64 * new_mss as u64 / self.mss as u64) as usize;
        if self.params.scale_pacing_rate_by_mss {
            self.pacing_rate = self.pacing_rate * (new_mss as f64 / self.mss as f64);
        }
        self.mss = new_mss;
    }

    pub fn on_packet_sent(
        &mut self,
        sent_time: Instant,
        bytes_in_flight: usize,
        packet_number: u64,
        bytes: usize,
        is_retransmissible: bool,
    ) {
        if bytes_in_flight == 0 && self.params.avoid_unnecessary_probe_rtt {
            self.on_exit_quiescence(sent_time);
        }

        let network_model = self.mode.network_model_mut();
        network_model.on_packet_sent(
            sent_time,
            bytes_in_flight,
            packet_number,
            bytes,
            is_retransmissible,
        );
    }

    pub fn on_congestion_event(
        &mut self,
        prior_in_flight: usize,
        event_time: Instant,
        acked_packets: &[Acked],
        lost_packets: &[Lost],
        least_unacked: u64,
    ) {
        let mut congestion_event = BBRv2CongestionEvent::new(
            event_time,
            self.cwnd,
            prior_in_flight,
            self.mode.is_probing_for_bandwidth(),
        );

        let network_model = self.mode.network_model_mut();
        network_model.on_congestion_event_start(
            acked_packets,
            lost_packets,
            &mut congestion_event,
            &self.params,
        );

        let mut mode_changes_allowed = MAX_MODE_CHANGES_PER_CONGESTION_EVENT;
        while mode_changes_allowed > 0
            && self.mode.do_on_congestion_event(
                prior_in_flight,
                event_time,
                acked_packets,
                lost_packets,
                &mut congestion_event,
                self.target_bytes_inflight(),
                &self.params,
                self.get_congestion_window(),
            )
        {
            mode_changes_allowed -= 1;
        }

        self.update_pacing_rate(congestion_event.bytes_acked);
        self.update_congestion_window(congestion_event.bytes_acked);

        let network_model = self.mode.network_model_mut();
        network_model.on_congestion_event_finish(least_unacked, &congestion_event);

        self.last_sample_is_app_limited = congestion_event.last_packet_send_state.is_app_limited;
        if !self.last_sample_is_app_limited {
            self.has_non_app_limited_sample = true;
        }

        if congestion_event.bytes_in_flight == 0 && self.params.avoid_unnecessary_probe_rtt {
            self.on_enter_quiescence(event_time);
        }
    }

    pub fn on_packet_neutered(&mut self, packet_number: u64) {
        self.mode
            .network_model_mut()
            .on_packet_neutered(packet_number);
    }

    pub fn on_packet_partially_neutered(&mut self, packet_number: u64, bytes: usize) {
        self.mode
            .network_model_mut()
            .on_packet_partially_neutered(packet_number, bytes);
    }

    pub fn record_partial_ack(&mut self, packet_number: u64, bytes: usize) {
        self.mode
            .network_model_mut()
            .record_partial_ack(packet_number, bytes);
    }

    pub fn on_app_limited(&mut self) {
        self.mode.network_model_mut().on_app_limited();
    }

    fn on_exit_quiescence(&mut self, now: Instant) {
        if let Some(last_quiescence_start) = self.last_quiescence_start.take() {
            self.mode
                .do_on_exit_quiescence(now, last_quiescence_start, &self.params);
        }
    }

    fn on_enter_quiescence(&mut self, time: Instant) {
        self.last_quiescence_start = Some(time);
    }

    fn get_target_congestion_window(&self, gain: f32) -> usize {
        let network_model = self.mode.network_model();
        network_model
            .bdp(network_model.bandwidth_estimate(), gain)
            .max(self.cwnd_limits.min())
    }

    fn target_bytes_inflight(&self) -> usize {
        let network_model = self.mode.network_model();
        let bdp = network_model.bdp1(network_model.bandwidth_estimate());
        bdp.min(self.get_congestion_window())
    }

    fn update_pacing_rate(&mut self, bytes_acked: usize) {
        let network_model = self.mode.network_model();
        let bandwidth_estimate = match network_model.bandwidth_estimate() {
            e if e == Bandwidth::zero() => return,
            e => e,
        };

        if network_model.total_bytes_acked() == bytes_acked {
            self.pacing_rate =
                Bandwidth::from_bytes_and_time_delta(self.cwnd, network_model.min_rtt());

            if let Some(pacing_rate) = self.params.initial_pacing_rate_bytes_per_second {
                let initial_pacing_rate = Bandwidth::from_bytes_per_second(pacing_rate);
                self.pacing_rate = self.pacing_rate.min(initial_pacing_rate);
            }

            return;
        }

        let target_rate = bandwidth_estimate * network_model.pacing_gain();
        if network_model.full_bandwidth_reached() {
            self.pacing_rate = target_rate;
            return;
        }

        if self.params.decrease_startup_pacing_at_end_of_round
            && network_model.pacing_gain() < self.params.startup_pacing_gain
        {
            self.pacing_rate = target_rate;
            return;
        }

        if self.params.bw_lo_mode != BwLoMode::Default && network_model.loss_events_in_round() > 0 {
            self.pacing_rate = target_rate;
            return;
        }

        self.pacing_rate = self.pacing_rate.max(target_rate);
    }

    fn update_congestion_window(&mut self, bytes_acked: usize) {
        let network_model = self.mode.network_model();
        let mut target_cwnd = self.get_target_congestion_window(network_model.cwnd_gain());

        let prior_cwnd = self.cwnd;
        if network_model.full_bandwidth_reached() {
            target_cwnd += network_model.max_ack_height();
            self.cwnd = target_cwnd.min(prior_cwnd + bytes_acked);
        } else if prior_cwnd < target_cwnd || prior_cwnd < 2 * self.initial_cwnd {
            self.cwnd = prior_cwnd + bytes_acked;
        }

        self.cwnd = self
            .mode
            .get_cwnd_limits(&self.params)
            .apply_limits(self.cwnd);
        self.cwnd = self.cwnd_limits.apply_limits(self.cwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bbr2::core::bandwidth_sampler::{Acked, Lost};

    const MSS: usize = 1200;
    const RTT: Duration = Duration::from_millis(50);
    /// Congestion window in packets used by [`new_bbr2`].
    const INITIAL_PACKETS: usize = 10;
    /// Upper bound in packets used by [`new_bbr2`].
    const MAX_PACKETS: usize = 200;

    fn new_bbr2(now: Instant, params: Option<&Bbr2Params>) -> BBRv2 {
        BBRv2::new(INITIAL_PACKETS, MAX_PACKETS, MSS, RTT, now, params)
    }

    /// Send `packets` packets and acknowledge all of them one RTT later.
    fn send_and_ack_round(
        bbr: &mut BBRv2,
        now: &mut Instant,
        packet_number: &mut u64,
        packets: usize,
    ) {
        send_round(bbr, now, packet_number, packets, 0);
    }

    /// Send `packets` packets, then mark `lost` of them as lost and acknowledge
    /// the rest one RTT later.
    fn send_round(
        bbr: &mut BBRv2,
        now: &mut Instant,
        packet_number: &mut u64,
        packets: usize,
        lost: usize,
    ) {
        let sent_at = *now;
        let first_packet = *packet_number + 1;
        let mut bytes_in_flight = 0;

        for _ in 0..packets {
            *packet_number += 1;
            bytes_in_flight += MSS;
            bbr.on_packet_sent(sent_at, bytes_in_flight, *packet_number, MSS, true);
        }

        *now += RTT;

        let lost_from = *packet_number - lost as u64 + 1;
        let acked: Vec<Acked> = (first_packet..lost_from)
            .map(|pkt_num| Acked {
                pkt_num,
                time_sent: sent_at,
            })
            .collect();
        let lost_packets: Vec<Lost> = (lost_from..=*packet_number)
            .map(|packet_number| Lost {
                packet_number,
                bytes_lost: MSS,
            })
            .collect();

        // `prior_in_flight` is everything that was in flight before this round
        // was acknowledged.
        bbr.on_congestion_event(bytes_in_flight, *now, &acked, &lost_packets, *packet_number);
    }

    #[test]
    fn starts_in_startup_with_the_initial_window() {
        let now = Instant::now();
        let bbr = new_bbr2(now, None);

        assert_eq!(bbr.state_str(), "bbr_startup");
        assert_eq!(bbr.get_congestion_window(), INITIAL_PACKETS * MSS);
        assert_eq!(bbr.get_congestion_window_in_packets(), INITIAL_PACKETS);
        assert_eq!(bbr.min_rtt(), RTT);
        assert!(bbr.pacing_rate() > Bandwidth::zero());
        assert!(bbr.can_send(0));
        assert!(!bbr.can_send(INITIAL_PACKETS * MSS));
    }

    #[test]
    fn leaves_startup_once_bandwidth_plateaus() {
        let start = Instant::now();
        let mut now = start;
        let mut packet_number = 0;
        let mut bbr = new_bbr2(start, None);

        assert_eq!(bbr.state_str(), "bbr_startup");

        // Every round delivers the same amount of data in the same time, so
        // the estimated bandwidth stops growing and BBRv2 has to declare the
        // pipe full and start probing bandwidth instead of ramping up.
        let mut state = bbr.state_str().to_owned();
        for _ in 0..12 {
            send_and_ack_round(&mut bbr, &mut now, &mut packet_number, INITIAL_PACKETS);
            state = bbr.state_str().to_owned();
        }

        assert!(
            state.starts_with("bbr_probe_bw"),
            "expected to leave Startup for ProbeBW, saw {state}"
        );
    }

    #[test]
    fn congestion_window_never_exceeds_its_upper_bound() {
        let start = Instant::now();
        let mut now = start;
        let mut packet_number = 0;
        let mut bbr = new_bbr2(start, None);

        for _ in 0..50 {
            send_and_ack_round(&mut bbr, &mut now, &mut packet_number, 32);
        }

        assert!(bbr.get_congestion_window() <= MAX_PACKETS * MSS);
    }

    #[test]
    fn losses_do_not_grow_the_congestion_window() {
        let start = Instant::now();
        let mut now = start;
        let mut packet_number = 0;
        let mut bbr = new_bbr2(start, None);

        for _ in 0..6 {
            send_and_ack_round(&mut bbr, &mut now, &mut packet_number, INITIAL_PACKETS);
        }
        let cwnd_before = bbr.get_congestion_window();

        // A round in which every packet is lost acknowledges no data at all,
        // so the window must not grow.
        for _ in 0..3 {
            send_round(
                &mut bbr,
                &mut now,
                &mut packet_number,
                INITIAL_PACKETS,
                INITIAL_PACKETS,
            );
        }

        assert!(bbr.get_congestion_window() <= cwnd_before);
    }

    #[test]
    fn custom_parameters_override_the_defaults() {
        let params = Bbr2Params {
            initial_pacing_rate_bytes_per_second: Some(2_000_000),
            probe_rtt_duration: Some(Duration::from_millis(150)),
            ..Default::default()
        };
        let bbr = new_bbr2(Instant::now(), Some(&params));

        assert_eq!(
            bbr.pacing_rate(),
            Bandwidth::from_bytes_per_second(2_000_000)
        );
        assert_eq!(bbr.state_str(), "bbr_startup");
    }

    #[test]
    fn updating_the_mss_scales_the_window() {
        let start = Instant::now();
        let mut bbr = new_bbr2(start, None);
        let cwnd_before = bbr.get_congestion_window();

        bbr.update_mss(1500);

        assert_eq!(bbr.get_congestion_window(), cwnd_before / MSS * 1500,);
        assert_eq!(bbr.get_congestion_window_in_packets(), INITIAL_PACKETS);
    }
}
