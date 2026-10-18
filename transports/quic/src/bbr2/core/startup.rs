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
    mode::{Mode, ModeImpl},
    network_model::BBRv2NetworkModel,
};

#[derive(Debug, Clone)]
pub(crate) struct Startup {
    pub(crate) model: BBRv2NetworkModel,
}

impl ModeImpl for Startup {
    fn state_str(&self) -> &'static str {
        "bbr_startup"
    }

    fn is_probing_for_bandwidth(&self) -> bool {
        true
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
        if self.model.full_bandwidth_reached() {
            return self.into_drain(event_time, Some(congestion_event), params);
        }

        if !congestion_event.end_of_round_trip {
            return Mode::Startup(self);
        }

        let has_bandwidth_growth = self.model.has_bandwidth_growth(congestion_event, params);

        let check_persistent_queue = params.max_startup_queue_rounds > 0 && !has_bandwidth_growth;
        if check_persistent_queue {
            self.model.check_persistent_queue(1.75, params);
        }

        let check_for_excessive_loss = !congestion_event.last_packet_send_state.is_app_limited
            && !has_bandwidth_growth
            && !self.model.full_bandwidth_reached();

        if check_for_excessive_loss {
            self.check_excessive_losses(congestion_event, params);
        }

        if self.model.full_bandwidth_reached() {
            self.into_drain(event_time, Some(congestion_event), params)
        } else {
            Mode::Startup(self)
        }
    }

    fn get_cwnd_limits(&self, _params: &Params) -> Limits<usize> {
        Limits {
            lo: 0,
            hi: self.model.inflight_lo(),
        }
    }

    fn on_exit_quiescence(
        self,
        _now: Instant,
        _quiescence_start_time: Instant,
        _params: &Params,
    ) -> Mode {
        Mode::Startup(self)
    }

    fn enter(
        &mut self,
        _now: Instant,
        _congestion_event: Option<&BBRv2CongestionEvent>,
        _params: &Params,
    ) {
        unreachable!("Enter should never be called for startup");
    }

    fn leave(&mut self, _now: Instant, _congestion_event: Option<&BBRv2CongestionEvent>) {
        self.model.clear_bandwidth_lo();
    }
}

impl Startup {
    fn into_drain(
        mut self,
        now: Instant,
        congestion_event: Option<&BBRv2CongestionEvent>,
        params: &Params,
    ) -> Mode {
        self.leave(now, congestion_event);
        let mut next_mode = Mode::drain(self.model, now);
        next_mode.enter(now, congestion_event, params);
        next_mode
    }

    fn check_excessive_losses(&mut self, congestion_event: &BBRv2CongestionEvent, params: &Params) {
        if self
            .model
            .is_inflight_too_high(congestion_event, params.startup_full_loss_count, params)
        {
            let mut new_inflight_hi = self.model.bdp0();
            if params.startup_loss_exit_use_max_delivered_for_inflight_hi {
                new_inflight_hi = self
                    .model
                    .max_bytes_delivered_in_round()
                    .max(new_inflight_hi);
            }
            self.model.set_inflight_hi(new_inflight_hi);
            self.model.set_full_bandwidth_reached();
        }
    }
}
