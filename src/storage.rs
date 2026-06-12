//! Persistent runtime config in NVS (survives reboots).

use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use crate::config;

const NAMESPACE: &str = "voicecfg";

#[derive(Clone, Debug)]
pub struct StoredConfig {
    pub ssid: String,
    pub pass: String,
    pub server: String,
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

    /// Returns the stored config, or `None` if no SSID has been provisioned yet.
    pub fn load(&self) -> Result<Option<StoredConfig>> {
        let mut sbuf = [0u8; 64];
        let ssid = self.nvs.get_str("ssid", &mut sbuf)?.unwrap_or("").to_string();
        if ssid.is_empty() {
            return Ok(None);
        }
        let mut pbuf = [0u8; 96];
        let pass = self.nvs.get_str("pass", &mut pbuf)?.unwrap_or("").to_string();
        let mut svbuf = [0u8; 96];
        let server = self
            .nvs
            .get_str("server", &mut svbuf)?
            .unwrap_or(config::DEFAULT_SERVER)
            .to_string();
        let volume = self.nvs.get_u8("volume")?.unwrap_or(config::DEFAULT_VOLUME);
        Ok(Some(StoredConfig {
            ssid,
            pass,
            server,
            volume,
        }))
    }

    pub fn save(&mut self, cfg: &StoredConfig) -> Result<()> {
        self.nvs.set_str("ssid", &cfg.ssid)?;
        self.nvs.set_str("pass", &cfg.pass)?;
        self.nvs.set_str("server", &cfg.server)?;
        self.nvs.set_u8("volume", cfg.volume)?;
        Ok(())
    }
}
