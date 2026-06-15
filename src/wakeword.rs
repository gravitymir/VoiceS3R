//! On-device "Hi ESP" wake word using Espressif's ESP-SR (WakeNet9).
//!
//! The WakeNet model ships in the `model` flash partition (see partitions.csv /
//! srmodels.bin). At boot we open it with `esp_srmodel_init("model")`, build a
//! single-microphone AFE (Audio Front End) pipeline with only WakeNet enabled
//! (AEC/SE/NS/VAD/AGC off to save CPU on the single mic), then repeatedly
//! `feed()` 16 kHz mono mic frames and `fetch()` the detection result.
//!
//! All of this is unsafe C FFI into the esp-sr component; the bindings come from
//! esp_sr_bindings.h via esp-idf-sys (see Cargo.toml extra_components).

use anyhow::{bail, Result};
use esp_idf_svc::sys::esp_sr as sys;
use log::info;

pub struct WakeWord {
    handle: *const sys::esp_afe_sr_iface_t,
    data: *mut sys::esp_afe_sr_data_t,
    /// Samples *per channel* the AFE wants per `feed()` call.
    feed_chunk: usize,
    /// Input channels in the feed frame (1 for the "M" single-mic format).
    channels: usize,
}

impl WakeWord {
    pub fn new() -> Result<Self> {
        unsafe {
            // 1. Open the speech-model partition (label must match partitions.csv).
            let models = sys::esp_srmodel_init(c"model".as_ptr());
            if models.is_null() || (*models).num <= 0 {
                bail!("esp_srmodel_init(\"model\") found no models (partition not flashed?)");
            }
            info!("esp-sr: {} model(s) in flash", (*models).num);

            // Pick the WakeNet model (prefix "wn") so we can log/confirm it.
            let wn_name = sys::esp_srmodel_filter(
                models,
                sys::ESP_WN_PREFIX.as_ptr() as *const core::ffi::c_char,
                core::ptr::null(),
            );
            if wn_name.is_null() {
                bail!("no WakeNet model in partition (expected wn9 Hi,ESP)");
            }
            info!(
                "esp-sr wakenet model: {}",
                core::ffi::CStr::from_ptr(wn_name).to_string_lossy()
            );

            // 2. Build an AFE config for a single mic ("M"), speech-recognition
            //    type, high-perf mode. Then strip everything but WakeNet.
            let cfg = sys::afe_config_init(
                c"M".as_ptr(),
                models,
                sys::afe_type_t_AFE_TYPE_SR,
                sys::afe_mode_t_AFE_MODE_HIGH_PERF,
            );
            if cfg.is_null() {
                bail!("afe_config_init returned null");
            }
            (*cfg).wakenet_init = true;
            (*cfg).aec_init = false;
            (*cfg).se_init = false;
            (*cfg).ns_init = false;
            (*cfg).vad_init = false;
            (*cfg).agc_init = false;

            // 3. Get the iface handle for this config and instantiate the pipeline.
            let handle = sys::esp_afe_handle_from_config(cfg);
            if handle.is_null() {
                bail!("esp_afe_handle_from_config returned null");
            }
            let create = (*handle)
                .create_from_config
                .ok_or_else(|| anyhow::anyhow!("afe.create_from_config is null"))?;
            let data = create(cfg);
            sys::afe_config_free(cfg);
            if data.is_null() {
                bail!("afe create_from_config returned null");
            }

            let feed_chunk = (*handle)
                .get_feed_chunksize
                .ok_or_else(|| anyhow::anyhow!("afe.get_feed_chunksize is null"))?(
                data
            ) as usize;
            let channels = (*handle)
                .get_feed_channel_num
                .ok_or_else(|| anyhow::anyhow!("afe.get_feed_channel_num is null"))?(
                data
            ) as usize;
            info!("esp-sr AFE ready: feed_chunk={feed_chunk} samples, channels={channels}");

            Ok(Self {
                handle,
                data,
                feed_chunk,
                channels,
            })
        }
    }

    /// Total i16 samples expected per `process()` call (chunk * channels).
    pub fn frame_len(&self) -> usize {
        self.feed_chunk * self.channels
    }

    /// Feed one frame of mic audio and return true if the wake word fired.
    /// `frame` must be exactly `frame_len()` i16 samples of 16 kHz mono PCM.
    pub fn process(&mut self, frame: &[i16]) -> bool {
        debug_assert_eq!(frame.len(), self.frame_len());
        unsafe {
            if let Some(feed) = (*self.handle).feed {
                feed(self.data, frame.as_ptr());
            }
            let fetch = match (*self.handle).fetch {
                Some(f) => f,
                None => return false,
            };
            let res = fetch(self.data);
            if res.is_null() {
                return false;
            }
            (*res).wakeup_state == sys::wakenet_state_t_WAKENET_DETECTED
        }
    }
}

impl Drop for WakeWord {
    fn drop(&mut self) {
        unsafe {
            if let Some(destroy) = (*self.handle).destroy {
                destroy(self.data);
            }
        }
    }
}
