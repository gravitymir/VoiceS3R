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
use crate::wakeword::{WakeWord, PERSONA_SOPHIA};

const VOL_NO_CHANGE: u8 = 0xFF;
const ENTER_SPEAKER: u8 = 0xFE;
/// Server→device reply byte: stay in continuous transcribe mode (record the next
/// utterance immediately, no wake word). The server tracks the mode; the device
/// keeps looping while it keeps receiving this byte.
const TRANSCRIBE: u8 = 0xFD;
/// Device→server header bit (OR'd into the persona byte): "button pressed — leave
/// transcribe mode". Sent once, with an empty utterance, when the user presses the
/// button during transcribe mode so the server can clear its state.
const TRANSCRIBE_EXIT: u8 = 0x80;
/// Server→device reply bytes that start a STREAMING transcribe session: the device
/// opens one long-lived connection to the transcribe-stream port and pushes the
/// mic continuously (the server segments + transcribes). LOCAL = on-PC Whisper,
/// EXTERNAL = OpenAI Realtime. The byte also tells the device which backend to
/// announce in the stream header.
const STREAM_LOCAL: u8 = 0xFC;
const STREAM_EXTERNAL: u8 = 0xFB;
/// Server→device reply byte for CODING mode: behave like a normal turn (idle, back
/// to wake-word/button), but use a longer trailing-silence tail for the next
/// recordings so the user can pause mid-command while thinking. Stays in effect
/// until any other reply byte (which resets to the normal tail).
const CODING: u8 = 0xFA;
/// Control bytes 128..=228 mean: set volume (byte − 128) AND enter speaker mode
/// — lets one response do a compound "volume N and speaker mode" command.
const SPEAKER_VOL_BASE: u8 = 128;

/// What the server's control byte asks the device to do after a turn.
enum Reply {
    /// Normal turn — return to wake-word listening.
    Done,
    /// Enter WiFi speaker mode.
    Speaker,
    /// Continuous transcribe — record the next utterance with no wake word.
    Transcribe,
    /// Open a streaming transcribe session (true = external/Realtime, false = local).
    Stream(bool),
    /// Button pressed during a keep-listening (transcribe/translate/etc.) turn —
    /// leave the mode and notify the server.
    ButtonExit,
    /// Coding mode — like Done (idle), but use the longer silence tail next time.
    Coding,
}
/// TCP port of the PC audio stream server (`pc_speaker`).
const SPEAKER_PORT: u16 = 9001;
/// TCP port the device streams the mic to for streaming transcription.
const TRANSCRIBE_STREAM_PORT: u16 = 9002;
/// Device→server stream header bit: external (Realtime) backend (else local).
const STREAM_BACKEND_EXTERNAL: u8 = 0x80;

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
    let transcribe_addr = format!("{host}:{TRANSCRIBE_STREAM_PORT}");

    info!("Voice assistant ready. Say 'Sophia' or 'Jarvis', or press the button.");
    let mut frame = vec![0i16; wakeword.frame_len()];
    let mut hb = 0u32;
    // Coding mode uses a longer trailing-silence tail; the flag persists across
    // turns (the server keeps it on by replying 0xFA) until any other reply.
    let mut long_tail = false;
    loop {
        // Manual trigger: a short button press is equivalent to the wake word
        // (a fallback). It uses the default persona (Sophia).
        if pressed(button) {
            wait_release(button);
            info!("button trigger");
            run_turn(audio, button, codec, store, server, &speaker_addr, &transcribe_addr, PERSONA_SOPHIA, &mut long_tail)?;
            continue;
        }

        audio.read_samples(&mut frame)?;

        hb += 1;
        if hb % 94 == 0 {
            // ~3 s heartbeat so the serial log shows the loop is alive.
            info!("listening for wake word...");
        }

        if let Some(persona) = wakeword.process(&frame) {
            run_turn(audio, button, codec, store, server, &speaker_addr, &transcribe_addr, persona, &mut long_tail)?;
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
    transcribe_addr: &str,
    persona: u8,
    long_tail: &mut bool,
) -> Result<()> {
    // In continuous transcribe mode the device records utterance after utterance
    // with no wake word (the server tracks the mode and keeps replying TRANSCRIBE).
    // Exit is the button: it notifies the server so it can clear its state.
    let mut transcribing = false;
    // After the button stops a mode we don't jump straight home — we listen for the
    // next command right away (no wake word), so the button doubles as "switch
    // mode". If that listen hears nothing within a short window, THEN we go home.
    // `quick_listen` marks such a post-stop turn (2 s no-speech window vs 4 s).
    let mut quick_listen = false;
    loop {
        // Button while in a keep-listening mode: clear the server mode, then listen
        // for a new command immediately.
        if transcribing && pressed(button) {
            wait_release(button);
            if let Err(e) = send_transcribe_exit(audio, server, persona) {
                warn!("transcribe exit notify failed: {e:?}");
            }
            transcribing = false;
            quick_listen = true;
            continue;
        }
        // Ack chirp ("I'm listening, speak now") — but NOT during continuous
        // transcribe: the beep bleeds into the mic and gets transcribed as "Beep"
        // on silent turns. Dictation is continuous, so no per-sentence cue needed.
        if !transcribing {
            audio.beep(1200, 60)?;
        }

        // Post-stop listen uses a short no-speech window so silence quickly falls
        // through to home; a normal / first turn gives the user longer.
        let start_ms = if quick_listen { 2000 } else { 4000 };
        quick_listen = false;

        match handle_command(
            audio, codec, store, server, persona, button, transcribing, start_ms, *long_tail,
        )
        .unwrap_or(Reply::Done)
        {
            // Button tapped mid-turn while in a keep-listening mode: clear the
            // server mode, then listen for the next command immediately (no wake
            // word). Caught inside the record/playback loop so a short tap isn't lost.
            Reply::ButtonExit => {
                wait_release(button);
                if let Err(e) = send_transcribe_exit(audio, server, persona) {
                    warn!("transcribe exit notify failed: {e:?}");
                }
                transcribing = false;
                quick_listen = true;
                continue;
            }
            // Enter / stay in continuous transcribe: record the next utterance
            // immediately with no wake word (exit only via the button, above).
            Reply::Transcribe => {
                transcribing = true;
                continue;
            }
            // Speaker mode: play the PC audio stream until the button is pressed.
            // A button press both EXITS speaker mode and immediately listens for the
            // next command; a closed stream returns to idle.
            Reply::Speaker => {
                transcribing = false;
                match speaker_session(audio, button, speaker_addr) {
                    Ok(true) => {
                        quick_listen = true; // button stopped audio -> listen for a command
                        continue;
                    }
                    Ok(false) => break, // stream ended
                    Err(e) => {
                        warn!("speaker mode failed: {e:?}");
                        break;
                    }
                }
            }
            // Streaming transcribe: open one long-lived connection and push the
            // mic continuously until the button is pressed, then back to idle.
            Reply::Stream(external) => {
                if let Err(e) = transcribe_stream_session(audio, button, transcribe_addr, persona, external) {
                    warn!("transcribe stream failed: {e:?}");
                }
                break;
            }
            // A normal command/answer (or a silent post-stop listen) — go home
            // (back to wake-word listening), resetting to the normal silence tail.
            Reply::Done => {
                *long_tail = false;
                break;
            }
            // Coding mode: idle like Done, but keep the long silence tail for the
            // next command (the server keeps replying 0xFA while in coding mode).
            Reply::Coding => {
                *long_tail = true;
                break;
            }
        }
    }
    info!("resuming wake-word listening");
    Ok(())
}

/// Record the spoken command (energy-based endpointing), stream it to the PC,
/// then play the response. Returns the `Reply` the server's control byte asks for.
///
/// Endpointing: once the mic energy crosses a speech threshold, the turn ends
/// after ~1.5 s of trailing silence. Caps guard against no-speech / runaways.
fn handle_command(
    audio: &mut Audio<'_>,
    codec: &mut Codec<'_>,
    store: &mut Store,
    server: &str,
    persona: u8,
    button: &PinDriver<'_, Gpio41, Input>,
    transcribing: bool,
    start_timeout_ms: usize,
    long_tail: bool,
) -> Result<Reply> {
    const FRAME_MS: usize = 32; // read_mono returns ~512 samples = 32 ms
    const MAX_FRAMES: usize = 30000 / FRAME_MS; // ~30 s hard cap (long dictation)
    let start_timeout = start_timeout_ms / FRAME_MS; // give up if silent this long
    // Trailing-silence tail that ends a turn: longer in coding mode so the user can
    // pause mid-command to think and then keep talking.
    let silence_end: usize = (if long_tail { 3000 } else { 1500 }) / FRAME_MS;
    const SPEECH_PEAK: i32 = 350; // quiet floor ~40, speech ~1000+

    info!("connecting to {server}");
    let mut stream = TcpStream::connect(server)?;
    stream.set_nodelay(true).ok();

    // Send the persona id (which wake word fired) as the FIRST byte, before any
    // audio. The server reads exactly this one byte, then the PCM — keeping the
    // 16-bit samples aligned. (Must match the server's read order.)
    stream.write_all(&[persona])?;

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
        // In a keep-listening mode, a button tap while we wait for / record the
        // next utterance means "leave the mode" — catch it here so a short press
        // isn't lost (the run_turn loop only re-checks the button between turns).
        if transcribing && pressed(button) {
            info!("button pressed during transcribe turn — exiting mode");
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(Reply::ButtonExit);
        }
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

        if spoke && silence >= silence_end {
            break;
        }
        if !spoke && frames >= start_timeout {
            info!("no speech detected ({start_timeout_ms} ms)");
            if !transcribing {
                // Nothing said on a normal / post-button listen → go straight home
                // (don't ship a few seconds of silence to the server).
                stream.shutdown(Shutdown::Both).ok();
                return Ok(Reply::Done);
            }
            break; // keep-listening mode: send the silence (server keeps / auto-exits)
        }
        if frames >= MAX_FRAMES {
            break;
        }
    }
    stream.flush()?;
    stream.shutdown(Shutdown::Write)?;
    info!("command sent ({sent} bytes), awaiting response");

    // --- Control header + response playback. ---
    // Await the 1-byte control header, playing a soft "thinking" loop meanwhile so
    // the device isn't dead-silent during the brain's few seconds of work.
    let mut ctrl = [0u8; 1];
    match await_control(audio, &mut stream)? {
        Some(b) => ctrl[0] = b,
        None => {
            info!("no response from server");
            return Ok(Reply::Done);
        }
    }
    let mut reply = Reply::Done;
    match ctrl[0] {
        VOL_NO_CHANGE => {}
        ENTER_SPEAKER => {
            reply = Reply::Speaker;
            info!("server requested speaker mode");
        }
        TRANSCRIBE => {
            reply = Reply::Transcribe;
            info!("server: continuous transcribe — listening for next utterance");
        }
        STREAM_LOCAL => {
            reply = Reply::Stream(false);
            info!("server: streaming transcribe (local)");
        }
        STREAM_EXTERNAL => {
            reply = Reply::Stream(true);
            info!("server: streaming transcribe (external/Realtime)");
        }
        CODING => {
            reply = Reply::Coding;
            info!("server: coding mode — idle, long silence tail");
        }
        // Compound: set volume AND enter speaker mode.
        v if v >= SPEAKER_VOL_BASE => {
            let vol = (v - SPEAKER_VOL_BASE).min(100);
            info!("server set volume = {vol} + speaker mode");
            if let Err(e) = codec.set_volume(vol) {
                warn!("set_volume failed: {e:?}");
            }
            store.set_volume(vol).ok();
            reply = Reply::Speaker;
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
        // Allow a button tap to interrupt playback and leave a keep-listening mode.
        if transcribing && pressed(button) {
            info!("button pressed during reply playback — exiting mode");
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(Reply::ButtonExit);
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        audio.write_mono(&buf[..n])?;
        total += n;
    }
    audio.write_silence().ok();
    info!("played {total} bytes of response audio");
    Ok(reply)
}

/// Wait for the server's 1-byte control header while playing a soft, looping
/// "thinking" cue so the device isn't dead-silent during the brain's work.
/// Non-blocking poll: the ~100 ms audio chunk paces the loop, and a fast reply is
/// caught on the next poll (so it plays little or no thinking sound). Returns the
/// control byte, or `None` if the connection closed with no response.
fn await_control(audio: &mut Audio<'_>, stream: &mut TcpStream) -> Result<Option<u8>> {
    const CHUNK: usize = 3200; // ~100 ms at 16 kHz mono 16-bit
    let think = config::PROMPT_THINKING;
    stream.set_nonblocking(true)?;
    let mut ctrl = [0u8; 1];
    let mut pos = 0usize;
    let result = loop {
        match stream.read(&mut ctrl) {
            Ok(0) => break None,           // server closed without replying
            Ok(_) => break Some(ctrl[0]),  // got the control header
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !think.is_empty() {
                    let end = (pos + CHUNK).min(think.len());
                    audio.write_mono(&think[pos..end])?; // blocks ~chunk -> paces poll
                    pos = if end >= think.len() { 0 } else { end };
                }
            }
            Err(e) => {
                stream.set_nonblocking(false).ok();
                return Err(e.into());
            }
        }
    };
    stream.set_nonblocking(false)?; // back to blocking for the response body
    Ok(result)
}

/// Tell the server to leave transcribe mode (the user pressed the button). Sends
/// one header byte with the exit marker and an empty utterance, then plays the
/// server's short spoken confirmation. Best-effort.
fn send_transcribe_exit(audio: &mut Audio<'_>, server: &str, persona: u8) -> Result<()> {
    info!("transcribe: button exit -> notifying server");
    audio.beep(700, 90).ok(); // low "off" cue — immediate feedback that the button registered
    let mut stream = TcpStream::connect(server)?;
    stream.set_nodelay(true).ok();
    stream.write_all(&[persona | TRANSCRIBE_EXIT])?; // header only, no audio
    stream.shutdown(Shutdown::Write)?;

    // Play the confirmation ("Transcribe mode off."), if any.
    let mut ctrl = [0u8; 1];
    if stream.read_exact(&mut ctrl).is_ok() {
        let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            audio.write_mono(&buf[..n])?;
        }
        audio.write_silence().ok();
    }
    Ok(())
}

/// Streaming transcribe session: open one long-lived connection to the
/// transcribe-stream port and push the mic continuously (16 kHz mono PCM) until
/// the button is pressed. The server segments the stream and transcribes it —
/// there are NO per-sentence reconnects or gaps, so nothing is lost. `external`
/// selects the server's backend (OpenAI Realtime vs local Whisper).
fn transcribe_stream_session(
    audio: &mut Audio<'_>,
    button: &PinDriver<'_, Gpio41, Input>,
    addr: &str,
    persona: u8,
    external: bool,
) -> Result<()> {
    info!("transcribe stream: connecting to {addr} (external={external})");
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true).ok();
    // Header: low bits = persona, high bit = backend (external/Realtime vs local).
    let header = persona | if external { STREAM_BACKEND_EXTERNAL } else { 0 };
    stream.write_all(&[header])?;

    let mut buf = [0u8; config::AUDIO_CHUNK_BYTES];

    // Drain the "on" prompt's echo so the first segment isn't the TTS tail.
    for _ in 0..(400 / 32) {
        let n = audio.read_mono(&mut buf)?;
        let mut peak = 0i32;
        for s in buf[..n].chunks_exact(2) {
            let v = (i16::from_le_bytes([s[0], s[1]]) as i32).abs();
            if v > peak {
                peak = v;
            }
        }
        if peak < 350 {
            break;
        }
    }

    info!("transcribe stream: streaming mic (press button to stop)");
    loop {
        if pressed(button) {
            wait_release(button);
            break;
        }
        let n = audio.read_mono(&mut buf)?;
        if n > 0 && stream.write_all(&buf[..n]).is_err() {
            info!("transcribe stream: server closed the connection");
            break;
        }
    }
    stream.shutdown(Shutdown::Both).ok();
    audio.beep(700, 90)?; // low "off" cue
    info!("transcribe stream: ended");
    Ok(())
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
