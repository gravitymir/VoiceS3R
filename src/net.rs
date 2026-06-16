//! Wake-word / button-triggered voice assistant over a raw TCP socket.
//!
//! Idle: the device listens for the on-device wake word ("Hi ESP"). A short
//! button press (press and release) is an equivalent manual trigger — useful
//! when the wake word isn't recognised. On either trigger the device:
//!   1. plays an ack chirp,
//!   2. records the spoken command (energy-based endpointing),
//!   3. streams 16 kHz/16-bit mono PCM to the PC, then half-closes (EOF),
//!   4. reads a 1-byte control header + response PCM and plays it:
//!        0xFF       = no change
//!        0x00..=100 = set speaker volume
//!        0xFE       = enter SPEAKER MODE
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
use crate::wakeword::WakeWord;

const VOL_NO_CHANGE: u8 = 0xFF;
const ENTER_SPEAKER: u8 = 0xFE;
/// Control bytes 128..=228 mean: set volume (byte − 128) AND enter speaker mode
/// — lets one response do a compound "volume N and speaker mode" command.
const SPEAKER_VOL_BASE: u8 = 128;
/// TCP port of the PC audio stream server (`pc_speaker`).
const SPEAKER_PORT: u16 = 9001;

/// Button is wired active-low (pressed = pin reads low).
fn pressed(button: &PinDriver<'_, Gpio41, Input>) -> bool {
    button.is_low()
}

/// Block until the button is released (debounce so one press == one action).
fn wait_release(button: &PinDriver<'_, Gpio41, Input>) {
    while pressed(button) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Hands-free wake-word assistant loop. Idle until the on-device wake word
/// ("Hi ESP") fires *or* the button is pressed, then run one spoken turn and
/// resume listening. Never returns under normal operation.
pub fn run_voice(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    wakeword: &mut WakeWord,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
) -> Result<()> {
    let host = server.split(':').next().unwrap_or(server);
    let speaker_addr = format!("{host}:{SPEAKER_PORT}");

    info!("Voice assistant ready. Say 'Hi ESP' (\"hi ee-ess-pee\"), or press the button.");
    let mut frame = vec![0i16; wakeword.frame_len()];
    let mut hb = 0u32;
    loop {
        // Manual trigger: a short button press is equivalent to the wake word
        // (a fallback for when the wake word isn't recognised).
        if pressed(button) {
            wait_release(button);
            info!("button trigger");
            run_turn(audio, button, codec, store, server, &speaker_addr)?;
            continue;
        }

        audio.read_samples(&mut frame)?;

        hb += 1;
        if hb % 94 == 0 {
            // ~3 s heartbeat so the serial log shows the loop is alive.
            info!("listening for wake word...");
        }

        if wakeword.process(&frame) {
            info!("wake word detected");
            run_turn(audio, button, codec, store, server, &speaker_addr)?;
        }
    }
}

/// One assistant turn: ack chirp, record + send the command, play the response,
/// and handle (sticky) speaker mode if the server requests it.
fn run_turn(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
    speaker_addr: &str,
) -> Result<()> {
    loop {
        audio.beep(1200, 60)?; // short ack chirp: "I'm listening, speak now"

        let enter_speaker = matches!(handle_command(audio, codec, store, server), Ok(true));
        if !enter_speaker {
            break; // a normal command/answer — return to wake-word listening
        }

        // Speaker mode: play the PC audio stream until the button is pressed. A
        // button press both EXITS speaker mode and immediately starts the next
        // command turn (chirp + listen); a closed stream returns to listening.
        match speaker_session(audio, button, speaker_addr) {
            Ok(true) => continue, // button -> loop back: chirp + record a command
            Ok(false) => break,   // stream ended
            Err(e) => {
                warn!("speaker mode failed: {e:?}");
                break;
            }
        }
    }
    info!("resuming wake-word listening");
    Ok(())
}

/// Record the spoken command (energy-based endpointing), stream it to the PC,
/// then play the response. Returns Ok(true) if the server asked for speaker mode.
///
/// Endpointing: once the mic energy crosses a speech threshold, the turn ends
/// after ~1.5 s of trailing silence. Caps guard against no-speech / runaways.
fn handle_command(
    audio: &mut Audio<'_>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
) -> Result<bool> {
    const FRAME_MS: usize = 32; // read_mono returns ~512 samples = 32 ms
    const MAX_FRAMES: usize = 30000 / FRAME_MS; // ~30 s hard cap (long dictation)
    const START_TIMEOUT: usize = 4000 / FRAME_MS; // give up if silent ~4 s
    const SILENCE_END: usize = 1500 / FRAME_MS; // ~1.5 s trailing silence ends turn
    const SPEECH_PEAK: i32 = 350; // quiet floor ~40, speech ~1000+

    info!("connecting to {server}");
    let mut stream = TcpStream::connect(server)?;
    stream.set_nodelay(true).ok();

    let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];

    // The ack chirp bleeds into the mic (speaker sits next to it + full-duplex
    // I2S), so the recording would otherwise capture the beep and end before the
    // user speaks. Drain mic frames until the beep echo falls below the speech
    // threshold (capped, so we don't eat the start of the command).
    for _ in 0..(600 / FRAME_MS) {
        let n = audio.read_mono(&mut buf)?;
        let mut peak = 0i32;
        for s in buf[..n].chunks_exact(2) {
            let v = (i16::from_le_bytes([s[0], s[1]]) as i32).abs();
            if v > peak {
                peak = v;
            }
        }
        if peak < SPEECH_PEAK {
            break; // echo gone, ready to record the command
        }
    }

    let mut spoke = false;
    let mut silence = 0usize;
    let mut frames = 0usize;
    let mut sent = 0usize;
    loop {
        let n = audio.read_mono(&mut buf)?;
        if n == 0 {
            continue;
        }
        let mut peak = 0i32;
        for s in buf[..n].chunks_exact(2) {
            let v = (i16::from_le_bytes([s[0], s[1]]) as i32).abs();
            if v > peak {
                peak = v;
            }
        }
        stream.write_all(&buf[..n])?;
        sent += n;
        frames += 1;

        if peak > SPEECH_PEAK {
            spoke = true;
            silence = 0;
        } else if spoke {
            silence += 1;
        }

        if spoke && silence >= SILENCE_END {
            break;
        }
        if !spoke && frames >= START_TIMEOUT {
            info!("no speech detected, ending turn");
            break;
        }
        if frames >= MAX_FRAMES {
            break;
        }
    }
    stream.flush()?;
    stream.shutdown(Shutdown::Write)?;
    info!("command sent ({sent} bytes), awaiting response");

    // --- Control header + response playback. ---
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
        // Compound: set volume AND enter speaker mode.
        v if v >= SPEAKER_VOL_BASE => {
            let vol = (v - SPEAKER_VOL_BASE).min(100);
            info!("server set volume = {vol} + speaker mode");
            if let Err(e) = codec.set_volume(vol) {
                warn!("set_volume failed: {e:?}");
            }
            store.set_volume(vol).ok();
            enter_speaker = true;
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

/// Play the PC audio stream until the button is pressed or the stream closes.
/// Returns Ok(true) if exited by a button press (caller starts a new command
/// turn), Ok(false) if the stream closed.
fn speaker_session(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    addr: &str,
) -> Result<bool> {
    info!("speaker mode: connecting to {addr}");
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];
    let mut by_button = false;
    loop {
        if pressed(button) {
            by_button = true;
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                info!("speaker stream closed");
                break;
            }
            Ok(n) => audio.write_mono(&buf[..n])?,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }

    drop(stream);
    audio.write_silence().ok();
    if by_button {
        wait_release(button); // so the same press isn't seen again by run_turn
    }
    info!("speaker mode exited ({})", if by_button { "button" } else { "stream closed" });
    Ok(by_button)
}
