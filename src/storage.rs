//! Persistent runtime config in NVS (survives reboots).
//!
//! Supports a list of up to [`MAX_WIFI`] WiFi networks; the device tries each in
//! order until one connects.

use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use crate::config;

const NAMESPACE: &str = "voicecfg";
pub const MAX_WIFI: usize = 5;

#[derive(Clone, Debug)]
pub struct WifiCred {
    pub ssid: String,
    pub pass: String,
    /// PC server `host:port` to use when connected to THIS network.
    pub server: String,
}

#[derive(Clone, Debug)]
pub struct StoredConfig {
    pub wifis: Vec<WifiCred>,
    pub volume: u8,
}

pub struct Store {
    nvs: EspNvs<NvsDefault>,
}

impl Store {
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        Ok(Self {
            nvs: EspNvs::new(partition, NAMESPACE, true)?,
        })
    }

    /// Returns the stored config, or `None` if no WiFi has been provisioned.
    pub fn load(&self) -> Result<Option<StoredConfig>> {
        let count = self.nvs.get_u8("n")?.unwrap_or(0) as usize;
        let mut wifis = Vec::new();
        for i in 0..count.min(MAX_WIFI) {
            let mut sb = [0u8; 64];
            let ssid = self
                .nvs
                .get_str(&format!("ssid{i}"), &mut sb)?
                .unwrap_or("")
                .to_string();
            if ssid.is_empty() {
                continue;
            }
            let mut pb = [0u8; 96];
            let pass = self
                .nvs
                .get_str(&format!("pass{i}"), &mut pb)?
                .unwrap_or("")
                .to_string();
            let mut svb = [0u8; 96];
            let server = self
                .nvs
                .get_str(&format!("srv{i}"), &mut svb)?
                .unwrap_or(config::DEFAULT_SERVER)
                .to_string();
            wifis.push(WifiCred { ssid, pass, server });
        }
        if wifis.is_empty() {
            return Ok(None);
        }
        let volume = self.nvs.get_u8("volume")?.unwrap_or(config::DEFAULT_VOLUME);
        Ok(Some(StoredConfig { wifis, volume }))
    }

    pub fn save(&mut self, cfg: &StoredConfig) -> Result<()> {
        let n = cfg.wifis.len().min(MAX_WIFI);
        self.nvs.set_u8("n", n as u8)?;
        for (i, w) in cfg.wifis.iter().take(MAX_WIFI).enumerate() {
            self.nvs.set_str(&format!("ssid{i}"), &w.ssid)?;
            self.nvs.set_str(&format!("pass{i}"), &w.pass)?;
            self.nvs.set_str(&format!("srv{i}"), &w.server)?;
        }
        self.nvs.set_u8("volume", cfg.volume)?;
        Ok(())
    }

    /// Persist just the speaker volume (used by runtime "set volume" commands).
    pub fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.nvs.set_u8("volume", volume)?;
        Ok(())
    }
}
