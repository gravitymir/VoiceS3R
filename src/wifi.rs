//! WiFi in either station (STA) or soft access-point (AP) mode.

use std::net::Ipv4Addr;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
};
use log::info;

pub struct WifiManager {
    wifi: BlockingWifi<EspWifi<'static>>,
}

impl WifiManager {
    pub fn new(
        modem: Modem,
        sysloop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> Result<Self> {
        let wifi = BlockingWifi::wrap(
            EspWifi::new(modem, sysloop.clone(), Some(nvs))?,
            sysloop,
        )?;
        Ok(Self { wifi })
    }

    fn stop_if_started(&mut self) -> Result<()> {
        if self.wifi.is_started()? {
            self.wifi.stop()?;
        }
        Ok(())
    }

    /// Try to join an access point. Returns the assigned IP on success.
    pub fn connect_sta(&mut self, ssid: &str, pass: &str) -> Result<Ipv4Addr> {
        self.stop_if_started()?;
        let auth = if pass.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };
        self.wifi
            .set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: ssid
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("SSID too long"))?,
                password: pass
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("password too long"))?,
                auth_method: auth,
                ..Default::default()
            }))?;
        self.wifi.start()?;
        info!("connecting to '{ssid}'...");
        self.wifi.connect()?;
        self.wifi.wait_netif_up()?;
        let ip = self.wifi.wifi().sta_netif().get_ip_info()?.ip;
        Ok(ip)
    }

    /// Raise a soft access point for provisioning. Returns the gateway IP.
    pub fn start_ap(&mut self, ssid: &str, pass: &str) -> Result<Ipv4Addr> {
        self.stop_if_started()?;
        let auth = if pass.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };
        self.wifi
            .set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
                ssid: ssid
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("AP SSID too long"))?,
                password: pass
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("AP password too long"))?,
                auth_method: auth,
                channel: 1,
                max_connections: 4,
                ..Default::default()
            }))?;
        self.wifi.start()?;
        let ip = self.wifi.wifi().ap_netif().get_ip_info()?.ip;
        Ok(ip)
    }
}
