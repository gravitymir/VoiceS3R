//! ATOM VoiceS3R firmware: push-to-talk voice terminal.
//!
//! Boot flow:
//!   1. Bring up I2S audio + ES8311 codec.
//!   2. Load WiFi credentials from NVS; if present, try to connect.
//!   3. If no creds / connect fails: play the "access point" prompt, raise the
//!      `VoiceS3R` SoftAP + setup page, wait for the user to submit WiFi + the PC
//!      server address, save, and connect.
//!   4. Play "ready for work", then run push-to-talk: hold the button to stream
//!      16 kHz mono mic audio to the PC over TCP and play the response back.
//!
//! (The diagnostic web panel lives in control.rs/control.html for bring-up.)

mod audio;
mod codec;
mod config;
mod net;
mod provision;
mod storage;
mod wifi;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{PinDriver, Pull};
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{info, warn};

use crate::audio::Audio;
use crate::codec::Codec;
use crate::storage::Store;
use crate::wifi::WifiManager;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // The default main-task stack is too small for WiFi + audio buffers, so run
    // the app on a thread with a generous stack.
    let handle = std::thread::Builder::new()
        .stack_size(48 * 1024)
        .spawn(app)?;
    handle.join().expect("app thread panicked")
}

fn app() -> Result<()> {
    info!("ATOM VoiceS3R booting...");

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;
    let pins = peripherals.pins;

    // I2S first so MCLK/BCLK run before the codec is configured.
    let mut audio = Audio::new(
        peripherals.i2s0,
        pins.gpio17, // BCLK / SCLK
        pins.gpio3,  // WS / LRCK
        pins.gpio11, // MCLK
        pins.gpio4,  // ASDOUT: mic in
        pins.gpio48, // DSDIN: speaker out
    )?;
    let mut codec = Codec::new(peripherals.i2c0, pins.gpio45, pins.gpio0, pins.gpio18)?;

    let mut store = Store::new(nvs_part.clone())?;
    let mut wifi = WifiManager::new(peripherals.modem, sysloop, nvs_part)?;

    // Try stored credentials first.
    let mut active = match store.load()? {
        Some(cfg) => match wifi.connect_sta(&cfg.ssid, &cfg.pass) {
            Ok(ip) => {
                info!("connected with stored creds, IP {ip}");
                Some(cfg)
            }
            Err(e) => {
                warn!("stored creds failed: {e:?}");
                None
            }
        },
        None => {
            info!("no stored WiFi credentials");
            None
        }
    };

    // Provisioning loop until we successfully connect.
    while active.is_none() {
        audio.play_pcm(config::PROMPT_AP)?;
        let cfg = provision::run_portal(&mut wifi)?;
        store.save(&cfg)?;
        match wifi.connect_sta(&cfg.ssid, &cfg.pass) {
            Ok(ip) => {
                info!("provisioned and connected, IP {ip}");
                active = Some(cfg);
            }
            Err(e) => warn!("could not connect with submitted creds: {e:?}"),
        }
    }

    let cfg = active.unwrap();
    codec.set_volume(cfg.volume)?;
    audio.play_pcm(config::PROMPT_READY)?;

    // Push-to-talk button (G41, active-low with internal pull-up).
    let mut button = PinDriver::input(pins.gpio41)?;
    button.set_pull(Pull::Up)?;

    net::run(&mut audio, &button, &cfg.server)?;
    Ok(())
}
