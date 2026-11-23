// Copyright 2025 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use cxx::UniquePtr;
use webrtc_sys::apm::ffi as sys_apm;

use crate::{RtcError, RtcErrorType};

pub struct AudioProcessingModule {
    sys_handle: UniquePtr<sys_apm::AudioProcessingModule>,
}

/// High-pass plus the libwebrtc noise suppressor. Echo cancellation stays off:
/// this path has no reverse stream, and the NPU has no userspace runtime here.
pub struct NoiseSuppressor {
    module: AudioProcessingModule,
}

impl NoiseSuppressor {
    pub fn new() -> Self {
        Self {
            module: AudioProcessingModule::new(false, false, true, true),
        }
    }

    /// One 10 ms mono frame. A rejected frame is left unchanged.
    pub fn process(&mut self, frame: &mut [i16], sample_rate: i32) -> bool {
        self.module.process_stream(frame, sample_rate, 1).is_ok()
    }
}

impl AudioProcessingModule {
    pub fn new(
        echo_canceller_enabled: bool,
        gain_controller_enabled: bool,
        high_pass_filter_enabled: bool,
        noise_suppression_enabled: bool,
    ) -> Self {
        Self {
            sys_handle: sys_apm::create_apm(
                echo_canceller_enabled,
                gain_controller_enabled,
                high_pass_filter_enabled,
                noise_suppression_enabled,
            ),
        }
    }

    pub fn process_stream(
        &mut self,
        data: &mut [i16],
        sample_rate: i32,
        num_channels: i32,
    ) -> Result<(), RtcError> {
        let samples_per_10ms = (sample_rate as usize / 100) * num_channels as usize;
        assert!(
            data.len() % samples_per_10ms == 0 && data.len() >= samples_per_10ms,
            "slice must have a multiple of 10ms worth of samples"
        );

        for chunk in data.chunks_mut(samples_per_10ms) {
            if unsafe {
                self.sys_handle.pin_mut().process_stream(
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    sample_rate,
                    num_channels,
                )
            } != 0
            {
                return Err(RtcError {
                    error_type: RtcErrorType::Internal,
                    message: "Failed to process stream".to_string(),
                });
            }
        }

        Ok(())
    }

    pub fn process_reverse_stream(
        &mut self,
        data: &mut [i16],
        sample_rate: i32,
        num_channels: i32,
    ) -> Result<(), RtcError> {
        let samples_per_10ms = (sample_rate as usize / 100) * num_channels as usize;
        assert!(
            data.len() % samples_per_10ms == 0 && data.len() >= samples_per_10ms,
            "slice must have a multiple of 10ms worth of samples"
        );

        for chunk in data.chunks_mut(samples_per_10ms) {
            if unsafe {
                self.sys_handle.pin_mut().process_reverse_stream(
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    sample_rate,
                    num_channels,
                )
            } != 0
            {
                return Err(RtcError {
                    error_type: RtcErrorType::Internal,
                    message: "Failed to process reverse stream".to_string(),
                });
            }
        }

        Ok(())
    }

    pub fn set_stream_delay_ms(&mut self, delay_ms: i32) -> Result<(), RtcError> {
        if self.sys_handle.pin_mut().set_stream_delay_ms(delay_ms) == 0 {
            Ok(())
        } else {
            Err(RtcError {
                error_type: RtcErrorType::Internal,
                message: "Failed to set stream delay".to_string(),
            })
        }
    }
}
