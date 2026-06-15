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
mod wakeword;
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

    // Boot speaker self-test: a beep at a known volume.
    codec.set_volume(85)?;
    audio.beep(1000, 250)?;
    info!("boot beep played (volume 85)");

    // Button (G41) — used to trigger a command and to exit PC-speaker mode.
    let mut button = PinDriver::input(pins.gpio41)?;
    button.set_pull(Pull::Up)?;

    let mut store = Store::new(nvs_part.clone())?;
    let mut wifi = WifiManager::new(peripherals.modem, sysloop, nvs_part)?;

    // Try stored networks first (each in turn until one connects). On success we
    // keep the config and the server address of the network we connected to.
    let mut active: Option<(storage::StoredConfig, String)> = None;
    match store.load()? {
        Some(cfg) => match connect_list(&mut wifi, &cfg) {
            Some(server) => active = Some((cfg, server)),
            None => warn!("no stored network reachable"),
        },
        None => info!("no stored WiFi credentials"),
    }

    // Provisioning loop until one of the submitted networks connects.
    while active.is_none() {
        audio.play_pcm(config::PROMPT_AP)?;
        let cfg = provision::run_portal(&mut wifi)?;
        store.save(&cfg)?;
        match connect_list(&mut wifi, &cfg) {
            Some(server) => active = Some((cfg, server)),
            None => warn!("none of the submitted networks connected; re-provisioning"),
        }
    }

    let (cfg, server) = active.unwrap();
    let vol = cfg.volume.max(70); // floor so a stuck-low stored value can't mute us
    info!("stored volume = {}, using {}", cfg.volume, vol);
    codec.set_volume(vol)?;
    audio.play_pcm(config::PROMPT_READY)?;
    info!("using PC server {server}");

    // Bring up the on-device wake word, then run the hands-free assistant loop.
    info!("initializing wake word...");
    let mut wakeword = wakeword::WakeWord::new()?;

    net::run_voice(
        &mut audio,
        &button,
        &mut wakeword,
        &mut codec,
        &mut store,
        &server,
    )?;
    Ok(())
}

/// Try each stored network in order; on the first that connects, return the PC
/// server address configured for that network.
fn connect_list(wifi: &mut WifiManager, cfg: &storage::StoredConfig) -> Option<String> {
    for w in &cfg.wifis {
        info!("trying WiFi '{}'", w.ssid);
        match wifi.connect_sta(&w.ssid, &w.pass) {
            Ok(ip) => {
                info!("connected to '{}', IP {ip}, server {}", w.ssid, w.server);
                return Some(w.server.clone());
            }
            Err(e) => warn!("'{}' failed: {e:?}", w.ssid),
        }
    }
    None
}
