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

use std::time::Instant;

use super::{
    bandwidth_sampler::{Acked, Lost},
    bbr2::{BBRv2CongestionEvent, Limits, Params},
    mode::{Cycle, Mode, ModeImpl},
    network_model::BBRv2NetworkModel,
};

#[derive(Debug, Clone)]
pub(crate) struct ProbeRTT {
    pub(crate) model: BBRv2NetworkModel,
    pub(crate) cycle: Cycle,
    exit_time: Option<Instant>,
}

impl ProbeRTT {
    pub(crate) fn new(model: BBRv2NetworkModel, cycle: Cycle) -> Self {
        ProbeRTT {
            model,
            cycle,
            exit_time: None,
        }
    }

    fn into_probe_bw(
        mut self,
        now: Instant,
        congestion_event: Option<&BBRv2CongestionEvent>,
        params: &Params,
    ) -> Mode {
        self.leave(now, congestion_event);
        let mut next_mode = Mode::probe_bw(self.model, self.cycle);
        next_mode.enter(now, congestion_event, params);
        next_mode
    }

    fn inflight_target(&self, params: &Params) -> usize {
        self.model.bdp(
            self.model.max_bandwidth(),
            params.probe_rtt_inflight_target_bdp_fraction,
        )
    }
}

impl ModeImpl for ProbeRTT {
    fn state_str(&self) -> &'static str {
        "bbr_probe_rtt"
    }

    fn is_probing_for_bandwidth(&self) -> bool {
        false
    }

    fn on_congestion_event(
        mut self,
        _prior_in_flight: usize,
        event_time: Instant,
        _acked_packets: &[Acked],
        _lost_packets: &[Lost],
        congestion_event: &mut BBRv2CongestionEvent,
        _target_bytes_inflight: usize,
        params: &Params,
        _cwnd: usize,
    ) -> Mode {
        match self.exit_time {
            None => {
                if congestion_event.bytes_in_flight <= self.inflight_target(params) {
                    self.exit_time = Some(congestion_event.event_time + params.probe_rtt_duration);
                }
                Mode::ProbeRTT(self)
            }
            Some(exit_time) => {
                if congestion_event.event_time > exit_time {
                    self.into_probe_bw(event_time, Some(congestion_event), params)
                } else {
                    Mode::ProbeRTT(self)
                }
            }
        }
    }

    fn get_cwnd_limits(&self, params: &Params) -> Limits<usize> {
        let inflight_upper_bound = self
            .model
            .inflight_lo()
            .min(self.model.inflight_hi_with_headroom(params));
        Limits::no_greater_than(inflight_upper_bound.min(self.inflight_target(params)))
    }

    fn on_exit_quiescence(
        self,
        now: Instant,
        _quiescence_start_time: Instant,
        params: &Params,
    ) -> Mode {
        match self.exit_time {
            None => self.into_probe_bw(now, None, params),
            Some(exit_time) if now > exit_time => self.into_probe_bw(now, None, params),
            Some(_) => Mode::ProbeRTT(self),
        }
    }

    fn enter(
        &mut self,
        _now: Instant,
        _congestion_event: Option<&BBRv2CongestionEvent>,
        params: &Params,
    ) {
        self.model.set_pacing_gain(params.probe_rtt_pacing_gain);
        self.model.set_cwnd_gain(params.probe_rtt_cwnd_gain);
        self.exit_time = None;
    }

    fn leave(&mut self, _now: Instant, _congestion_event: Option<&BBRv2CongestionEvent>) {}
}
