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

// Implements Kathleen Nichols' algorithm for tracking the minimum (or maximum)
// estimate of a stream of samples over some fixed time interval.

use std::ops::{Div, Sub};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample<T, I> {
    sample: T,
    time: I,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowedFilter<T, I, D> {
    window_length: D,
    estimates: [Option<Sample<T, I>>; 3],
}

impl<T, I, D> WindowedFilter<T, I, D>
where
    T: Ord + Copy,
    I: Sub<I, Output = D> + Copy,
    D: Ord + Div<usize, Output = D> + Copy,
{
    pub(crate) fn new(window_length: D) -> Self {
        WindowedFilter {
            window_length,
            estimates: [None, None, None],
        }
    }

    pub(crate) fn reset(&mut self, new_sample: T, new_time: I) {
        let sample = Some(Sample {
            sample: new_sample,
            time: new_time,
        });

        self.estimates = [sample, sample, sample];
    }

    pub(crate) fn get_best(&self) -> Option<T> {
        self.estimates[0].as_ref().map(|e| e.sample)
    }

    pub(crate) fn get_second_best(&self) -> Option<T> {
        self.estimates[1].as_ref().map(|e| e.sample)
    }

    pub(crate) fn get_third_best(&self) -> Option<T> {
        self.estimates[2].as_ref().map(|e| e.sample)
    }

    pub(crate) fn clear(&mut self) {
        self.estimates = [None, None, None];
    }

    pub(crate) fn update(&mut self, new_sample: T, new_time: I) {
        if match &self.estimates[0] {
            None => true,
            Some(best) if new_sample > best.sample => true,
            _ => new_time - self.estimates[2].as_ref().unwrap().time > self.window_length,
        } {
            return self.reset(new_sample, new_time);
        }

        if new_sample > self.estimates[1].unwrap().sample {
            self.estimates[1] = Some(Sample {
                sample: new_sample,
                time: new_time,
            });
            self.estimates[2] = self.estimates[1];
        } else if new_sample > self.estimates[2].unwrap().sample {
            self.estimates[2] = Some(Sample {
                sample: new_sample,
                time: new_time,
            });
        }

        if new_time - self.estimates[0].unwrap().time > self.window_length {
            self.estimates[0] = self.estimates[1];
            self.estimates[1] = self.estimates[2];
            self.estimates[2] = Some(Sample {
                sample: new_sample,
                time: new_time,
            });
            if new_time - self.estimates[0].unwrap().time > self.window_length {
                self.estimates[0] = self.estimates[1];
                self.estimates[1] = self.estimates[2];
            }
            return;
        }

        if self.estimates[1].unwrap().sample == self.estimates[0].unwrap().sample
            && new_time - self.estimates[1].unwrap().time > self.window_length / 4
        {
            self.estimates[1] = Some(Sample {
                sample: new_sample,
                time: new_time,
            });
            self.estimates[2] = self.estimates[1];
            return;
        }

        if self.estimates[2].unwrap().sample == self.estimates[1].unwrap().sample
            && new_time - self.estimates[2].unwrap().time > self.window_length / 2
        {
            self.estimates[2] = Some(Sample {
                sample: new_sample,
                time: new_time,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_basic() {
        let mut filter: WindowedFilter<u64, usize, usize> = WindowedFilter::new(100);
        assert_eq!(filter.get_best(), None);

        filter.update(10, 0);
        assert_eq!(filter.get_best(), Some(10));

        filter.update(20, 10);
        assert_eq!(filter.get_best(), Some(20));

        filter.update(15, 40);
        assert_eq!(filter.get_best(), Some(20));

        // After window expires (10 + 100 = 110)
        filter.update(5, 120);
        assert_eq!(filter.get_best(), Some(15));
    }
}
