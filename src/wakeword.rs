//! On-device dual wake word ("Sophia" + "Jarvis") using Espressif's ESP-SR
//! (WakeNet9). Both models ship in the `model` flash partition (srmodels.bin).
//!
//! At boot we open the partition with `esp_srmodel_init("model")`, build a
//! single-microphone AFE (Audio Front End) with WakeNet enabled for BOTH models
//! (wakenet_model_name + wakenet_model_name_2), then repeatedly `feed()` 16 kHz
//! mono mic frames and `fetch()`. On a hit, `wakenet_model_index` says which name
//! was spoken, so the caller can pick the matching persona.
//!
//! All of this is unsafe C FFI into the esp-sr component; the bindings come from
//! esp_sr_bindings.h via esp-idf-sys (see Cargo.toml extra_components).

use anyhow::{bail, Result};
use core::ffi::c_char;
use esp_idf_svc::sys::esp_sr as sys;
use log::info;

/// Which wake word fired (also the persona id sent to the PC server).
pub const PERSONA_SOPHIA: u8 = 0;
pub const PERSONA_JARVIS: u8 = 1;

pub struct WakeWord {
    handle: *const sys::esp_afe_sr_iface_t,
    data: *mut sys::esp_afe_sr_data_t,
    feed_chunk: usize,
    channels: usize,
    /// 1-based `wakenet_model_index` that corresponds to Jarvis (or -1 if Jarvis
    /// isn't loaded). Determined definitively from `add_wakenet_model`'s return,
    /// so the persona mapping never depends on the AFE's internal model order.
    jarvis_index: i32,
}

impl WakeWord {
    pub fn new() -> Result<Self> {
        unsafe {
            let models = sys::esp_srmodel_init(c"model".as_ptr());
            if models.is_null() || (*models).num <= 0 {
                bail!("esp_srmodel_init(\"model\") found no models (partition not flashed?)");
            }
            info!("esp-sr: {} model(s) in flash", (*models).num);

            // Find each wake word's model by name keyword (prefix "wn" + name).
            let sophia = sys::esp_srmodel_filter(models, c"wn".as_ptr(), c"sophia".as_ptr());
            let jarvis = sys::esp_srmodel_filter(models, c"wn".as_ptr(), c"jarvis".as_ptr());
            if sophia.is_null() {
                bail!("Sophia WakeNet model (wn9_sophia_tts) not in partition");
            }
            log_name("wake word 1 (Sophia)", sophia);
            if jarvis.is_null() {
                info!("Jarvis WakeNet model not found — running Sophia only");
            } else {
                log_name("wake word 2 (Jarvis)", jarvis);
            }

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
            // Load Sophia as the single config wake word (becomes wakenet index 1);
            // Jarvis is added explicitly below so we learn its real index.
            (*cfg).wakenet_model_name = sophia;
            (*cfg).wakenet_model_name_2 = core::ptr::null_mut();

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

            // Add Jarvis as a second wakenet. add_wakenet_model returns the model
            // count after addition, i.e. this model's 1-based index — the only
            // reliable way to map a detection back to the right persona (the AFE
            // does not necessarily index models in the order we configured them).
            let mut jarvis_index: i32 = -1;
            if !jarvis.is_null() {
                if let Some(add) = (*handle).add_wakenet_model {
                    let count = add(data, jarvis as *const _);
                    if count > 0 {
                        jarvis_index = count;
                        info!("esp-sr: added Jarvis wakenet -> index {jarvis_index}");
                    } else {
                        info!("esp-sr: add_wakenet_model(jarvis) failed ({count}) — Sophia only");
                    }
                } else {
                    info!("esp-sr: afe.add_wakenet_model is null — Sophia only");
                }
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
            info!("esp-sr AFE ready (dual wake word): feed_chunk={feed_chunk}, channels={channels}");

            Ok(Self {
                handle,
                data,
                feed_chunk,
                channels,
                jarvis_index,
            })
        }
    }

    /// Total i16 samples expected per `process()` call (chunk * channels).
    pub fn frame_len(&self) -> usize {
        self.feed_chunk * self.channels
    }

    /// Feed one frame; return Some(persona) if a wake word fired, else None.
    /// `frame` must be exactly `frame_len()` i16 samples of 16 kHz mono PCM.
    pub fn process(&mut self, frame: &[i16]) -> Option<u8> {
        debug_assert_eq!(frame.len(), self.frame_len());
        unsafe {
            if let Some(feed) = (*self.handle).feed {
                feed(self.data, frame.as_ptr());
            }
            let fetch = (*self.handle).fetch?;
            let res = fetch(self.data);
            if res.is_null() || (*res).wakeup_state != sys::wakenet_state_t_WAKENET_DETECTED {
                return None;
            }
            // Map the detected wakenet index to a persona using Jarvis's known
            // index (from add_wakenet_model); everything else is Sophia.
            let idx = (*res).wakenet_model_index;
            let persona = if idx == self.jarvis_index {
                PERSONA_JARVIS
            } else {
                PERSONA_SOPHIA
            };
            let name = if persona == PERSONA_JARVIS { "Jarvis" } else { "Sophia" };
            info!("wake word detected: model_index={idx} -> {name} (persona {persona})");
            Some(persona)
        }
    }
}

fn log_name(label: &str, name: *const c_char) {
    unsafe {
        info!(
            "esp-sr {label}: {}",
            core::ffi::CStr::from_ptr(name).to_string_lossy()
        );
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
