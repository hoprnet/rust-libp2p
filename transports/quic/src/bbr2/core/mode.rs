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

use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

use super::{
    bandwidth_sampler::{Acked, Lost},
    bbr2::{BBRv2CongestionEvent, Limits, Params},
    drain::Drain,
    network_model::BBRv2NetworkModel,
    probe_bw::ProbeBW,
    probe_rtt::ProbeRTT,
    startup::Startup,
};

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CyclePhase {
    #[default]
    NotStarted,
    Up,
    Down,
    Cruise,
    Refill,
}

impl CyclePhase {
    pub(crate) fn pacing_gain(&self, params: &Params) -> f32 {
        match self {
            CyclePhase::Up => params.probe_bw_probe_up_pacing_gain,
            CyclePhase::Down => params.probe_bw_probe_down_pacing_gain,
            _ => params.probe_bw_default_pacing_gain,
        }
    }

    pub(crate) fn cwnd_gain(&self, params: &Params) -> f32 {
        match self {
            CyclePhase::Up => params.probe_bw_up_cwnd_gain,
            _ => params.probe_bw_cwnd_gain,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cycle {
    pub(crate) start_time: Instant,
    pub(crate) phase: CyclePhase,
    pub(crate) rounds_in_phase: usize,
    pub(crate) phase_start_time: Instant,
    pub(crate) rounds_since_probe: usize,
    pub(crate) probe_wait_time: Option<Duration>,
    pub(crate) probe_up_rounds: usize,
    pub(crate) probe_up_bytes: Option<usize>,
    pub(crate) probe_up_acked: usize,
    #[allow(dead_code)]
    pub(crate) probe_up_app_limited_since_inflight_hi_limited: bool,
    pub(crate) has_advanced_max_bw: bool,
    pub(crate) is_sample_from_probing: bool,
    pub(crate) last_cycle_probed_too_high: bool,
    pub(crate) last_cycle_stopped_risky_probe: bool,
}

impl Cycle {
    pub(crate) fn new(now: Instant) -> Self {
        Cycle {
            start_time: now,
            phase_start_time: now,
            phase: CyclePhase::NotStarted,
            rounds_in_phase: 0,
            rounds_since_probe: 0,
            probe_wait_time: None,
            probe_up_rounds: 0,
            probe_up_bytes: None,
            probe_up_acked: 0,
            probe_up_app_limited_since_inflight_hi_limited: false,
            has_advanced_max_bw: false,
            is_sample_from_probing: false,
            last_cycle_probed_too_high: false,
            last_cycle_stopped_risky_probe: false,
        }
    }
}

pub(crate) trait ModeImpl: Debug {
    fn state_str(&self) -> &'static str;

    fn enter(
        &mut self,
        now: Instant,
        congestion_event: Option<&BBRv2CongestionEvent>,
        params: &Params,
    );

    fn leave(&mut self, now: Instant, congestion_event: Option<&BBRv2CongestionEvent>);

    fn is_probing_for_bandwidth(&self) -> bool;

    #[allow(clippy::too_many_arguments)]
    fn on_congestion_event(
        self,
        prior_in_flight: usize,
        event_time: Instant,
        acked_packets: &[Acked],
        lost_packets: &[Lost],
        congestion_event: &mut BBRv2CongestionEvent,
        target_bytes_inflight: usize,
        params: &Params,
        cwnd: usize,
    ) -> Mode;

    fn get_cwnd_limits(&self, params: &Params) -> Limits<usize>;

    fn on_exit_quiescence(
        self,
        now: Instant,
        quiescence_start_time: Instant,
        params: &Params,
    ) -> Mode;
}

#[derive(Debug, Clone)]
pub(crate) struct Placeholder;

#[derive(Debug, Clone)]
pub(crate) enum Mode {
    Startup(Startup),
    Drain(Drain),
    ProbeBW(ProbeBW),
    ProbeRTT(ProbeRTT),
    Placeholder(Placeholder),
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Placeholder(Placeholder)
    }
}

impl Mode {
    pub(crate) fn startup(model: BBRv2NetworkModel) -> Self {
        Mode::Startup(Startup { model })
    }

    pub(crate) fn drain(model: BBRv2NetworkModel, now: Instant) -> Self {
        Mode::Drain(Drain {
            model,
            cycle: Cycle::new(now),
        })
    }

    pub(crate) fn probe_bw(model: BBRv2NetworkModel, cycle: Cycle) -> Self {
        Mode::ProbeBW(ProbeBW { model, cycle })
    }

    pub(crate) fn probe_rtt(model: BBRv2NetworkModel, cycle: Cycle) -> Self {
        Mode::ProbeRTT(ProbeRTT::new(model, cycle))
    }

    pub(crate) fn network_model(&self) -> &BBRv2NetworkModel {
        match self {
            Mode::Startup(s) => &s.model,
            Mode::Drain(d) => &d.model,
            Mode::ProbeBW(p) => &p.model,
            Mode::ProbeRTT(r) => &r.model,
            Mode::Placeholder(_) => unreachable!("network_model on placeholder"),
        }
    }

    pub(crate) fn network_model_mut(&mut self) -> &mut BBRv2NetworkModel {
        match self {
            Mode::Startup(s) => &mut s.model,
            Mode::Drain(d) => &mut d.model,
            Mode::ProbeBW(p) => &mut p.model,
            Mode::ProbeRTT(r) => &mut r.model,
            Mode::Placeholder(_) => unreachable!("network_model_mut on placeholder"),
        }
    }

    pub(crate) fn is_probing_for_bandwidth(&self) -> bool {
        match self {
            Mode::Startup(s) => s.is_probing_for_bandwidth(),
            Mode::Drain(d) => d.is_probing_for_bandwidth(),
            Mode::ProbeBW(p) => p.is_probing_for_bandwidth(),
            Mode::ProbeRTT(r) => r.is_probing_for_bandwidth(),
            Mode::Placeholder(_) => false,
        }
    }

    pub(crate) fn state_str(&self) -> &'static str {
        match self {
            Mode::Startup(s) => s.state_str(),
            Mode::Drain(d) => d.state_str(),
            Mode::ProbeBW(p) => p.state_str(),
            Mode::ProbeRTT(r) => r.state_str(),
            Mode::Placeholder(_) => "placeholder",
        }
    }

    pub(crate) fn get_cwnd_limits(&self, params: &Params) -> Limits<usize> {
        match self {
            Mode::Startup(s) => s.get_cwnd_limits(params),
            Mode::Drain(d) => d.get_cwnd_limits(params),
            Mode::ProbeBW(p) => p.get_cwnd_limits(params),
            Mode::ProbeRTT(r) => r.get_cwnd_limits(params),
            Mode::Placeholder(_) => Limits {
                lo: 0,
                hi: usize::MAX,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn do_on_congestion_event(
        &mut self,
        prior_in_flight: usize,
        event_time: Instant,
        acked_packets: &[Acked],
        lost_packets: &[Lost],
        congestion_event: &mut BBRv2CongestionEvent,
        target_bytes_inflight: usize,
        params: &Params,
        cwnd: usize,
    ) -> bool {
        let mode_before = std::mem::discriminant(self);

        let current = std::mem::take(self);
        *self = match current {
            Mode::Startup(s) => s.on_congestion_event(
                prior_in_flight,
                event_time,
                acked_packets,
                lost_packets,
                congestion_event,
                target_bytes_inflight,
                params,
                cwnd,
            ),
            Mode::Drain(d) => d.on_congestion_event(
                prior_in_flight,
                event_time,
                acked_packets,
                lost_packets,
                congestion_event,
                target_bytes_inflight,
                params,
                cwnd,
            ),
            Mode::ProbeBW(p) => p.on_congestion_event(
                prior_in_flight,
                event_time,
                acked_packets,
                lost_packets,
                congestion_event,
                target_bytes_inflight,
                params,
                cwnd,
            ),
            Mode::ProbeRTT(r) => r.on_congestion_event(
                prior_in_flight,
                event_time,
                acked_packets,
                lost_packets,
                congestion_event,
                target_bytes_inflight,
                params,
                cwnd,
            ),
            Mode::Placeholder(_) => unreachable!("do_on_congestion_event on placeholder"),
        };

        mode_before != std::mem::discriminant(self)
    }

    pub(crate) fn do_on_exit_quiescence(
        &mut self,
        now: Instant,
        quiescence_start_time: Instant,
        params: &Params,
    ) {
        let current = std::mem::take(self);
        *self = match current {
            Mode::Startup(s) => s.on_exit_quiescence(now, quiescence_start_time, params),
            Mode::Drain(d) => d.on_exit_quiescence(now, quiescence_start_time, params),
            Mode::ProbeBW(p) => p.on_exit_quiescence(now, quiescence_start_time, params),
            Mode::ProbeRTT(r) => r.on_exit_quiescence(now, quiescence_start_time, params),
            Mode::Placeholder(_) => Mode::Placeholder(Placeholder),
        };
    }

    pub(crate) fn enter(
        &mut self,
        now: Instant,
        congestion_event: Option<&BBRv2CongestionEvent>,
        params: &Params,
    ) {
        match self {
            Mode::Startup(s) => s.enter(now, congestion_event, params),
            Mode::Drain(d) => d.enter(now, congestion_event, params),
            Mode::ProbeBW(p) => p.enter(now, congestion_event, params),
            Mode::ProbeRTT(r) => r.enter(now, congestion_event, params),
            Mode::Placeholder(_) => {}
        }
    }

    #[allow(dead_code)]
    pub(crate) fn leave(&mut self, now: Instant, congestion_event: Option<&BBRv2CongestionEvent>) {
        match self {
            Mode::Startup(s) => s.leave(now, congestion_event),
            Mode::Drain(d) => d.leave(now, congestion_event),
            Mode::ProbeBW(p) => p.leave(now, congestion_event),
            Mode::ProbeRTT(r) => r.leave(now, congestion_event),
            Mode::Placeholder(_) => {}
        }
    }
}
