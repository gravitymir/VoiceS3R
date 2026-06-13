//! Push-to-talk audio streaming over a raw TCP socket.
//!
//! Protocol (one TCP connection per utterance):
//!   1. Device connects to the configured server.
//!   2. While the button is held, device streams 16 kHz/16-bit mono PCM up.
//!   3. On release, device half-closes the write side (server sees EOF = end of
//!      utterance), then transcribes -> LLM -> TTS on the PC.
//!   4. Server replies with a 1-byte control header (0xFF = no change, else a new
//!      speaker volume 0..=100) followed by the response PCM, which the device
//!      plays until EOF.
//!
//! Half-duplex keeps it simple and avoids mic/speaker echo.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

use anyhow::Result;
use esp_idf_svc::hal::gpio::{Gpio41, Input, PinDriver};
use log::{info, warn};

use crate::audio::Audio;
use crate::codec::Codec;
use crate::config;
use crate::storage::Store;

/// Response control byte meaning "leave the volume unchanged".
const VOL_NO_CHANGE: u8 = 0xFF;

/// Button is wired active-low (pressed = pin reads low).
fn pressed(button: &PinDriver<'_, Gpio41, Input>) -> bool {
    button.is_low()
}

/// Main interaction loop. Never returns under normal operation.
pub fn run(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
) -> Result<()> {
    info!("Ready. Hold the button to talk to {}.", server);
    loop {
        if pressed(button) {
            if let Err(e) = handle_utterance(audio, button, codec, store, server) {
                warn!("utterance failed: {e:?}");
            }
            // Wait for release so one press == one utterance.
            while pressed(button) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn handle_utterance(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
) -> Result<()> {
    info!("connecting to {}", server);
    let mut stream = TcpStream::connect(server)?;
    stream.set_nodelay(true).ok();

    // --- Uplink: stream mic audio (16 kHz mono) while the button is held. ---
    let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];
    let mut sent = 0usize;
    while pressed(button) {
        let n = audio.read_mono(&mut buf)?;
        if n > 0 {
            stream.write_all(&buf[..n])?;
            sent += n;
        }
    }
    stream.flush()?;
    // Signal end-of-utterance to the server.
    stream.shutdown(Shutdown::Write)?;
    info!("utterance sent ({sent} bytes), awaiting response");

    // --- Control header: 1 byte. 0xFF = no change, else new volume 0..=100. ---
    let mut ctrl = [0u8; 1];
    if stream.read_exact(&mut ctrl).is_err() {
        info!("no response from server");
        return Ok(());
    }
    if ctrl[0] != VOL_NO_CHANGE {
        let vol = ctrl[0].min(100);
        info!("server set volume = {vol}");
        if let Err(e) = codec.set_volume(vol) {
            warn!("set_volume failed: {e:?}");
        }
        store.set_volume(vol).ok(); // persist across reboots
    }

    // --- Downlink: play the response PCM (16 kHz mono) until the server closes. ---
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        audio.write_mono(&buf[..n])?;
        total += n;
    }
    audio.write_silence().ok();
    info!("played {} bytes of response audio", total);
    Ok(())
}
