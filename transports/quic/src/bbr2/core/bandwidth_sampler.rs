// Copyright (c) 2016 The Chromium Authors. All rights reserved.
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

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::{bandwidth::Bandwidth, windowed_filter::WindowedFilter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acked {
    pub pkt_num: u64,
    pub time_sent: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lost {
    pub packet_number: u64,
    pub bytes_lost: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ConnectionStateMap<T> {
    packet_map: VecDeque<(u64, Option<T>)>,
}

impl<T> Default for ConnectionStateMap<T> {
    fn default() -> Self {
        ConnectionStateMap {
            packet_map: VecDeque::new(),
        }
    }
}

impl<T> ConnectionStateMap<T> {
    pub(crate) fn insert(&mut self, pkt_num: u64, val: T) {
        if let Some((last_pkt, _)) = self.packet_map.back() {
            if pkt_num <= *last_pkt {
                return;
            }
        }

        self.packet_map.push_back((pkt_num, Some(val)));
    }

    pub(crate) fn take(&mut self, pkt_num: u64) -> Option<T> {
        let first = self.packet_map.front()?;
        if first.0 == pkt_num {
            return self.packet_map.pop_front().and_then(|(_, v)| v);
        }

        let ret = match self.packet_map.binary_search_by_key(&pkt_num, |&(n, _)| n) {
            Ok(found) => self.packet_map.get_mut(found).and_then(|(_, v)| v.take()),
            Err(_) => None,
        };

        while let Some((_, None)) = self.packet_map.front() {
            self.packet_map.pop_front();
        }

        ret
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn peek(&self, pkt_num: u64) -> Option<&T> {
        match self.packet_map.binary_search_by_key(&pkt_num, |&(n, _)| n) {
            Ok(found) => self.packet_map.get(found).and_then(|(_, v)| v.as_ref()),
            Err(_) => None,
        }
    }

    pub(crate) fn remove_obsolete(&mut self, least_acked: u64) {
        while match self.packet_map.front() {
            Some(&(p, _)) if p < least_acked => {
                self.packet_map.pop_front();
                true
            }
            _ => false,
        } {}
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConnectionStateOnSentPacket {
    pub sent_time: Instant,
    pub size: usize,
    pub total_bytes_sent_at_last_acked_packet: usize,
    pub last_acked_packet_sent_time: Instant,
    pub last_acked_packet_ack_time: Instant,
    pub send_time_state: SendTimeState,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SendTimeState {
    pub is_valid: bool,
    pub is_app_limited: bool,
    pub total_bytes_sent: usize,
    pub total_bytes_acked: usize,
    pub total_bytes_lost: usize,
    pub bytes_in_flight: usize,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
struct ExtraAckedEvent {
    extra_acked: usize,
    bytes_acked: usize,
    time_delta: Duration,
    round: usize,
}

#[derive(Debug)]
struct BandwidthSample {
    bandwidth: Bandwidth,
    rtt: Duration,
    send_rate: Option<Bandwidth>,
    ack_rate: Bandwidth,
    state_at_send: SendTimeState,
}

#[derive(Debug, Clone, Copy)]
struct AckPoint {
    ack_time: Instant,
    total_bytes_acked: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct RecentAckPoints {
    ack_points: [Option<AckPoint>; 2],
}

impl RecentAckPoints {
    fn update(&mut self, ack_time: Instant, total_bytes_acked: usize) {
        assert!(total_bytes_acked >= self.ack_points[1].map(|p| p.total_bytes_acked).unwrap_or(0));

        self.ack_points[0] = self.ack_points[1];
        self.ack_points[1] = Some(AckPoint {
            ack_time,
            total_bytes_acked,
        });
    }

    fn clear(&mut self) {
        self.ack_points = Default::default();
    }

    fn most_recent(&self) -> Option<AckPoint> {
        self.ack_points[1]
    }

    fn less_recent_point(&self, choose_a0_point_fix: bool) -> Option<AckPoint> {
        if choose_a0_point_fix {
            self.ack_points[0]
                .filter(|ack_point| ack_point.total_bytes_acked > 0)
                .or(self.ack_points[1])
        } else {
            self.ack_points[0].or(self.ack_points[1])
        }
    }
}

#[derive(Debug, Clone)]
struct MaxAckHeightTracker {
    max_ack_height_filter: WindowedFilter<ExtraAckedEvent, usize, usize>,
    aggregation_epoch_start_time: Option<Instant>,
    aggregation_epoch_bytes: usize,
    last_sent_packet_number_before_epoch: u64,
    num_ack_aggregation_epochs: u64,
    ack_aggregation_bandwidth_threshold: f64,
    start_new_aggregation_epoch_after_full_round: bool,
    reduce_extra_acked_on_bandwidth_increase: bool,
}

impl MaxAckHeightTracker {
    fn new(window: usize, overestimate_avoidance: bool) -> Self {
        MaxAckHeightTracker {
            max_ack_height_filter: WindowedFilter::new(window),
            aggregation_epoch_start_time: None,
            aggregation_epoch_bytes: 0,
            last_sent_packet_number_before_epoch: 0,
            num_ack_aggregation_epochs: 0,
            ack_aggregation_bandwidth_threshold: if overestimate_avoidance { 2.0 } else { 1.0 },
            start_new_aggregation_epoch_after_full_round: true,
            reduce_extra_acked_on_bandwidth_increase: true,
        }
    }

    #[allow(dead_code)]
    fn reset(&mut self, new_height: usize, new_time: usize) {
        self.max_ack_height_filter.reset(
            ExtraAckedEvent {
                extra_acked: new_height,
                bytes_acked: 0,
                time_delta: Duration::ZERO,
                round: new_time,
            },
            new_time,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        bandwidth_estimate: Bandwidth,
        is_new_max_bandwidth: bool,
        round_trip_count: usize,
        last_sent_packet_number: u64,
        last_acked_packet_number: u64,
        ack_time: Instant,
        bytes_acked: usize,
    ) -> usize {
        let mut force_new_epoch = false;

        if self.reduce_extra_acked_on_bandwidth_increase && is_new_max_bandwidth {
            let mut best = self.max_ack_height_filter.get_best().unwrap_or_default();
            let mut second_best = self
                .max_ack_height_filter
                .get_second_best()
                .unwrap_or_default();
            let mut third_best = self
                .max_ack_height_filter
                .get_third_best()
                .unwrap_or_default();
            self.max_ack_height_filter.clear();

            let expected_bytes_acked =
                bandwidth_estimate.to_bytes_per_period(best.time_delta) as usize;
            if expected_bytes_acked < best.bytes_acked {
                best.extra_acked = best.bytes_acked - expected_bytes_acked;
                self.max_ack_height_filter.update(best, best.round);
            }

            let expected_bytes_acked =
                bandwidth_estimate.to_bytes_per_period(second_best.time_delta) as usize;
            if expected_bytes_acked < second_best.bytes_acked {
                second_best.extra_acked = second_best.bytes_acked - expected_bytes_acked;
                self.max_ack_height_filter
                    .update(second_best, second_best.round);
            }

            let expected_bytes_acked =
                bandwidth_estimate.to_bytes_per_period(third_best.time_delta) as usize;
            if expected_bytes_acked < third_best.bytes_acked {
                third_best.extra_acked = third_best.bytes_acked - expected_bytes_acked;
                self.max_ack_height_filter
                    .update(third_best, third_best.round);
            }
        }

        if self.start_new_aggregation_epoch_after_full_round
            && last_acked_packet_number > self.last_sent_packet_number_before_epoch
        {
            force_new_epoch = true;
        }

        let epoch_start_time = match self.aggregation_epoch_start_time {
            Some(time) if !force_new_epoch => time,
            _ => {
                self.aggregation_epoch_bytes = bytes_acked;
                self.aggregation_epoch_start_time = Some(ack_time);
                self.last_sent_packet_number_before_epoch = last_sent_packet_number;
                self.num_ack_aggregation_epochs += 1;
                return 0;
            }
        };

        let aggregation_delta = ack_time.duration_since(epoch_start_time);
        let expected_bytes_acked =
            bandwidth_estimate.to_bytes_per_period(aggregation_delta) as usize;

        if self.aggregation_epoch_bytes
            <= (self.ack_aggregation_bandwidth_threshold * expected_bytes_acked as f64) as usize
        {
            self.aggregation_epoch_bytes = bytes_acked;
            self.aggregation_epoch_start_time = Some(ack_time);
            self.last_sent_packet_number_before_epoch = last_sent_packet_number;
            self.num_ack_aggregation_epochs += 1;
            return 0;
        }

        self.aggregation_epoch_bytes += bytes_acked;
        let extra_bytes_acked = self.aggregation_epoch_bytes - expected_bytes_acked;

        let new_event = ExtraAckedEvent {
            extra_acked: extra_bytes_acked,
            bytes_acked: self.aggregation_epoch_bytes,
            time_delta: aggregation_delta,
            round: 0,
        };

        self.max_ack_height_filter
            .update(new_event, round_trip_count);
        extra_bytes_acked
    }
}

#[derive(Default, Debug, Clone)]
pub struct CongestionEventSample {
    pub sample_max_bandwidth: Option<Bandwidth>,
    pub sample_is_app_limited: bool,
    pub sample_rtt: Option<Duration>,
    pub sample_max_inflight: usize,
    pub last_packet_send_state: SendTimeState,
    pub extra_acked: usize,
    pub sample_max_send_rate: Option<Bandwidth>,
    pub sample_max_ack_rate: Option<Bandwidth>,
}

#[derive(Debug, Clone)]
pub struct BandwidthSampler {
    total_bytes_sent: usize,
    total_bytes_acked: usize,
    total_bytes_lost: usize,
    total_bytes_neutered: usize,
    last_sent_packet: u64,
    last_acked_packet: u64,
    is_app_limited: bool,
    last_acked_packet_ack_time: Instant,
    total_bytes_sent_at_last_acked_packet: usize,
    last_acked_packet_sent_time: Instant,
    recent_ack_points: RecentAckPoints,
    a0_candidates: VecDeque<AckPoint>,
    pub(crate) connection_state_map: ConnectionStateMap<ConnectionStateOnSentPacket>,
    max_ack_height_tracker: MaxAckHeightTracker,
    end_of_app_limited_phase: Option<u64>,
    overestimate_avoidance: bool,
    choose_a0_point_fix: bool,
    limit_max_ack_height_tracker_by_send_rate: bool,
    total_bytes_acked_after_last_ack_event: usize,
}

impl BandwidthSampler {
    pub fn new(
        now: Instant,
        max_height_tracker_window_length: usize,
        overestimate_avoidance: bool,
        choose_a0_point_fix: bool,
    ) -> Self {
        BandwidthSampler {
            total_bytes_sent: 0,
            total_bytes_acked: 0,
            total_bytes_lost: 0,
            total_bytes_neutered: 0,
            total_bytes_sent_at_last_acked_packet: 0,
            last_acked_packet_sent_time: now,
            last_acked_packet_ack_time: now,
            is_app_limited: true,
            connection_state_map: ConnectionStateMap::default(),
            max_ack_height_tracker: MaxAckHeightTracker::new(
                max_height_tracker_window_length,
                overestimate_avoidance,
            ),
            total_bytes_acked_after_last_ack_event: 0,
            overestimate_avoidance,
            choose_a0_point_fix,
            limit_max_ack_height_tracker_by_send_rate: false,
            last_sent_packet: 0,
            last_acked_packet: 0,
            recent_ack_points: RecentAckPoints::default(),
            a0_candidates: VecDeque::new(),
            end_of_app_limited_phase: None,
        }
    }

    pub fn is_app_limited(&self) -> bool {
        self.is_app_limited
    }

    pub fn on_packet_sent(
        &mut self,
        sent_time: Instant,
        packet_number: u64,
        bytes: usize,
        bytes_in_flight: usize,
        has_retransmittable_data: bool,
    ) {
        self.last_sent_packet = packet_number;

        if !has_retransmittable_data {
            return;
        }

        self.total_bytes_sent += bytes;

        if bytes_in_flight == 0 {
            self.last_acked_packet_ack_time = sent_time;
            if self.overestimate_avoidance {
                self.recent_ack_points.clear();
                self.recent_ack_points
                    .update(sent_time, self.total_bytes_acked);
                self.a0_candidates.clear();
                self.a0_candidates
                    .push_back(self.recent_ack_points.most_recent().unwrap());
            }

            self.total_bytes_sent_at_last_acked_packet = self.total_bytes_sent;
            self.last_acked_packet_sent_time = sent_time;
        }

        let state_on_sent = ConnectionStateOnSentPacket {
            sent_time,
            size: bytes,
            total_bytes_sent_at_last_acked_packet: self.total_bytes_sent_at_last_acked_packet,
            last_acked_packet_sent_time: self.last_acked_packet_sent_time,
            last_acked_packet_ack_time: self.last_acked_packet_ack_time,
            send_time_state: SendTimeState {
                is_valid: true,
                is_app_limited: self.is_app_limited,
                total_bytes_sent: self.total_bytes_sent,
                total_bytes_acked: self.total_bytes_acked,
                total_bytes_lost: self.total_bytes_lost,
                bytes_in_flight: bytes_in_flight + bytes,
            },
        };

        self.connection_state_map
            .insert(packet_number, state_on_sent);
    }

    pub fn on_packet_neutered(&mut self, packet_number: u64) {
        if let Some(pkt) = self.connection_state_map.take(packet_number) {
            self.total_bytes_neutered += pkt.size;
        }
    }

    pub fn on_congestion_event(
        &mut self,
        ack_time: Instant,
        acked_packets: &[Acked],
        lost_packets: &[Lost],
        mut max_bandwidth: Option<Bandwidth>,
        est_bandwidth_upper_bound: Bandwidth,
        round_trip_count: usize,
    ) -> CongestionEventSample {
        let mut last_lost_packet_send_state = SendTimeState::default();
        let mut last_acked_packet_send_state = SendTimeState::default();
        let mut last_lost_packet_num = 0u64;
        let mut last_acked_packet_num = 0u64;

        for packet in lost_packets {
            let send_state = self.on_packet_lost(packet.packet_number, packet.bytes_lost);
            if send_state.is_valid {
                last_lost_packet_send_state = send_state;
                last_lost_packet_num = packet.packet_number;
            }
        }

        if acked_packets.is_empty() {
            return CongestionEventSample {
                last_packet_send_state: last_lost_packet_send_state,
                ..Default::default()
            };
        }

        let mut event_sample = CongestionEventSample::default();
        let mut max_send_rate = None;
        let mut max_ack_rate = None;

        for packet in acked_packets {
            let sample = match self.on_packet_acknowledged(ack_time, packet.pkt_num) {
                Some(sample) if sample.state_at_send.is_valid => sample,
                _ => continue,
            };

            last_acked_packet_send_state = sample.state_at_send;
            last_acked_packet_num = packet.pkt_num;

            event_sample.sample_rtt = Some(
                sample
                    .rtt
                    .min(*event_sample.sample_rtt.get_or_insert(sample.rtt)),
            );

            if Some(sample.bandwidth) > event_sample.sample_max_bandwidth {
                event_sample.sample_max_bandwidth = Some(sample.bandwidth);
                event_sample.sample_is_app_limited = sample.state_at_send.is_app_limited;
            }
            max_send_rate = max_send_rate.max(sample.send_rate);
            max_ack_rate = max_ack_rate.max(Some(sample.ack_rate));

            let inflight_sample =
                self.total_bytes_acked - last_acked_packet_send_state.total_bytes_acked;
            if inflight_sample > event_sample.sample_max_inflight {
                event_sample.sample_max_inflight = inflight_sample;
            }
        }

        if !last_lost_packet_send_state.is_valid {
            event_sample.last_packet_send_state = last_acked_packet_send_state;
        } else if !last_acked_packet_send_state.is_valid {
            event_sample.last_packet_send_state = last_lost_packet_send_state;
        } else {
            event_sample.last_packet_send_state = if last_acked_packet_num > last_lost_packet_num {
                last_acked_packet_send_state
            } else {
                last_lost_packet_send_state
            };
        }

        let is_new_max_bandwidth = event_sample.sample_max_bandwidth > max_bandwidth;
        max_bandwidth = event_sample.sample_max_bandwidth.max(max_bandwidth);

        if self.limit_max_ack_height_tracker_by_send_rate {
            max_bandwidth = max_bandwidth.max(max_send_rate);
        }

        let bandwidth_estimate = if let Some(max_bandwidth) = max_bandwidth {
            max_bandwidth.min(est_bandwidth_upper_bound)
        } else {
            est_bandwidth_upper_bound
        };

        event_sample.extra_acked =
            self.on_ack_event_end(bandwidth_estimate, is_new_max_bandwidth, round_trip_count);

        event_sample.sample_max_send_rate = max_send_rate;
        event_sample.sample_max_ack_rate = max_ack_rate;

        event_sample
    }

    fn on_packet_lost(&mut self, packet_number: u64, bytes_lost: usize) -> SendTimeState {
        let mut send_time_state = SendTimeState::default();

        self.total_bytes_lost += bytes_lost;
        if let Some(state) = self.connection_state_map.take(packet_number) {
            send_time_state = state.send_time_state;
            send_time_state.is_valid = true;
        }

        send_time_state
    }

    fn on_ack_event_end(
        &mut self,
        bandwidth_estimate: Bandwidth,
        is_new_max_bandwidth: bool,
        round_trip_count: usize,
    ) -> usize {
        let newly_acked_bytes =
            self.total_bytes_acked - self.total_bytes_acked_after_last_ack_event;

        if newly_acked_bytes == 0 {
            return 0;
        }

        self.total_bytes_acked_after_last_ack_event = self.total_bytes_acked;
        let extra_acked = self.max_ack_height_tracker.update(
            bandwidth_estimate,
            is_new_max_bandwidth,
            round_trip_count,
            self.last_sent_packet,
            self.last_acked_packet,
            self.last_acked_packet_ack_time,
            newly_acked_bytes,
        );

        if self.overestimate_avoidance && extra_acked == 0 {
            if let Some(less_recent) = self
                .recent_ack_points
                .less_recent_point(self.choose_a0_point_fix)
            {
                self.a0_candidates.push_back(less_recent);
            }
        }

        extra_acked
    }

    fn on_packet_acknowledged(
        &mut self,
        ack_time: Instant,
        packet_number: u64,
    ) -> Option<BandwidthSample> {
        self.last_acked_packet = packet_number;
        let sent_packet = self.connection_state_map.take(packet_number)?;

        self.total_bytes_acked += sent_packet.size;
        self.total_bytes_sent_at_last_acked_packet = sent_packet.send_time_state.total_bytes_sent;
        self.last_acked_packet_sent_time = sent_packet.sent_time;
        self.last_acked_packet_ack_time = ack_time;
        if self.overestimate_avoidance {
            self.recent_ack_points
                .update(ack_time, self.total_bytes_acked);
        }

        if self.is_app_limited
            && (self.end_of_app_limited_phase.is_none()
                || Some(packet_number) > self.end_of_app_limited_phase)
        {
            self.is_app_limited = false;
        }

        let send_rate = if sent_packet.sent_time > sent_packet.last_acked_packet_sent_time {
            Some(Bandwidth::from_bytes_and_time_delta(
                sent_packet.send_time_state.total_bytes_sent
                    - sent_packet.total_bytes_sent_at_last_acked_packet,
                sent_packet.sent_time - sent_packet.last_acked_packet_sent_time,
            ))
        } else {
            None
        };

        let a0 = if self.overestimate_avoidance {
            Self::choose_a0_point(
                &mut self.a0_candidates,
                sent_packet.send_time_state.total_bytes_acked,
                self.choose_a0_point_fix,
            )
        } else {
            None
        };

        let a0 = a0.unwrap_or(AckPoint {
            ack_time: sent_packet.last_acked_packet_ack_time,
            total_bytes_acked: sent_packet.send_time_state.total_bytes_acked,
        });

        if ack_time <= a0.ack_time {
            return None;
        }

        let ack_rate = Bandwidth::from_bytes_and_time_delta(
            self.total_bytes_acked - a0.total_bytes_acked,
            ack_time.duration_since(a0.ack_time),
        );

        let bandwidth = if let Some(send_rate) = send_rate {
            send_rate.min(ack_rate)
        } else {
            ack_rate
        };

        let rtt = ack_time.duration_since(sent_packet.sent_time);

        Some(BandwidthSample {
            bandwidth,
            rtt,
            send_rate,
            ack_rate,
            state_at_send: SendTimeState {
                is_valid: true,
                ..sent_packet.send_time_state
            },
        })
    }

    fn choose_a0_point(
        a0_candidates: &mut VecDeque<AckPoint>,
        total_bytes_acked: usize,
        choose_a0_point_fix: bool,
    ) -> Option<AckPoint> {
        if a0_candidates.is_empty() {
            return None;
        }

        while let Some(candidate) = a0_candidates.get(1) {
            if candidate.total_bytes_acked > total_bytes_acked {
                if choose_a0_point_fix {
                    break;
                } else {
                    return Some(*candidate);
                }
            }
            a0_candidates.pop_front();
        }

        Some(a0_candidates[0])
    }

    pub fn total_bytes_acked(&self) -> usize {
        self.total_bytes_acked
    }

    pub fn total_bytes_lost(&self) -> usize {
        self.total_bytes_lost
    }

    pub fn total_bytes_sent(&self) -> usize {
        self.total_bytes_sent
    }

    pub fn max_ack_height(&self) -> Option<usize> {
        self.max_ack_height_tracker
            .max_ack_height_filter
            .get_best()
            .map(|b| b.extra_acked)
    }

    pub fn on_app_limited(&mut self) {
        self.is_app_limited = true;
        self.end_of_app_limited_phase = Some(self.last_sent_packet);
    }

    pub fn remove_obsolete_packets(&mut self, least_acked: u64) {
        self.connection_state_map.remove_obsolete(least_acked);
    }
}
