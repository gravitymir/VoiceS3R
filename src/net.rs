//! Push-to-talk audio streaming over a raw TCP socket, plus a PC-speaker mode.
//!
//! Push-to-talk (one TCP connection per utterance):
//!   1. Device connects to the configured server.
//!   2. While the button is held, it streams 16 kHz/16-bit mono PCM up.
//!   3. On release it half-closes the write side (server sees EOF), then the PC
//!      does STT -> LLM/agent -> TTS.
//!   4. Server replies with a 1-byte control header followed by response PCM:
//!        0xFF       = no change
//!        0x00..=100 = set speaker volume
//!        0xFE       = enter SPEAKER MODE
//!      The device plays the response PCM, then acts on the control byte.
//!
//! Speaker mode: the device connects to the PC's audio stream (host:9001,
//! `pc_speaker`) and plays it continuously until the button is pressed.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::hal::gpio::{Gpio41, Input, PinDriver};
use log::{info, warn};

use crate::audio::Audio;
use crate::codec::Codec;
use crate::config;
use crate::storage::Store;

const VOL_NO_CHANGE: u8 = 0xFF;
const ENTER_SPEAKER: u8 = 0xFE;
/// TCP port of the PC audio stream server (`pc_speaker`).
const SPEAKER_PORT: u16 = 9001;

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
    let host = server.split(':').next().unwrap_or(server);
    let speaker_addr = format!("{host}:{SPEAKER_PORT}");

    info!("Ready. Hold the button to talk to {server}.");
    loop {
        if pressed(button) {
            match handle_utterance(audio, button, codec, store, server) {
                Ok(true) => {
                    if let Err(e) = speaker_mode(audio, button, &speaker_addr) {
                        warn!("speaker mode failed: {e:?}");
                    }
                }
                Ok(false) => {}
                Err(e) => warn!("utterance failed: {e:?}"),
            }
            // Wait for release so one press == one action.
            while pressed(button) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Returns Ok(true) if the server asked us to enter speaker mode.
fn handle_utterance(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
) -> Result<bool> {
    info!("connecting to {server}");
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
    stream.shutdown(Shutdown::Write)?;
    info!("utterance sent ({sent} bytes), awaiting response");

    // --- Control header (1 byte). ---
    let mut ctrl = [0u8; 1];
    if stream.read_exact(&mut ctrl).is_err() {
        info!("no response from server");
        return Ok(false);
    }
    let mut enter_speaker = false;
    match ctrl[0] {
        VOL_NO_CHANGE => {}
        ENTER_SPEAKER => {
            enter_speaker = true;
            info!("server requested speaker mode");
        }
        v => {
            let vol = v.min(100);
            info!("server set volume = {vol}");
            if let Err(e) = codec.set_volume(vol) {
                warn!("set_volume failed: {e:?}");
            }
            store.set_volume(vol).ok();
        }
    }

    // --- Downlink: play the response PCM until the server closes. ---
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
    info!("played {total} bytes of response audio");
    Ok(enter_speaker)
}

/// Play the PC audio stream until the button is pressed.
fn speaker_mode(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    addr: &str,
) -> Result<()> {
    info!("speaker mode: connecting to {addr}");
    let mut stream = TcpStream::connect(addr)?;
    // Short read timeout so we can poll the button between reads.
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];
    loop {
        if pressed(button) {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // server closed
            Ok(n) => audio.write_mono(&buf[..n])?,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // no data this interval — loop and re-check the button
            }
            Err(e) => return Err(e.into()),
        }
    }
    audio.write_silence().ok();
    info!("speaker mode ended");
    Ok(())
}
