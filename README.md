# VoiceS3R

Rust firmware for the **M5Stack ATOM VoiceS3R** (SKU C126-ECHO) — a hands-free
voice assistant. It listens for two on-device wake words — **"Sophia"** and
**"Jarvis"** (ESP-SR / WakeNet9, runs locally) — or a button press, then streams
your voice over WiFi to a PC and plays back the spoken reply. The PC side does the
STT, the AI brain, and the TTS. Which name you say picks the persona: **Sophia**
answers as a woman (nova voice), **Jarvis** as a man (onyx voice) — no "switch to"
command needed.

**Pairs with the PC server — install & run it from
[ServerVoiceS3R](https://github.com/gravitymir/ServerVoiceS3R)** (Whisper STT →
Claude/OpenAI brain → OpenAI/Windows TTS; also a "skills" agent, WiFi speaker
mode, a hands-free voice coding mode, and a **continuous dictation / transcribe**
mode — OpenAI Realtime, optionally typed straight into the focused field, exit by
button). See that repo's README for setup.

```
ATOM VoiceS3R  ──(hold button)── 16 kHz mono PCM ──TCP──▶  PC server
   speaker     ◀────────────────  16 kHz mono PCM ──TCP──   (STT → LLM → TTS)
```

## Hardware

- **MCU:** ESP32-S3-PICO-1-N8R8 (dual-core LX7 @240 MHz, 8 MB flash, 8 MB octal PSRAM, WiFi)
- **Codec:** ES8311 (mono, I2S, configured over I2C)
- **Mic:** MEMS, 65 dB SNR · **Amp:** NS4150B class-D → 8 Ω 1 W speaker
- **Button:** G41 · **IR:** G47 · **Port.A (HY2.0):** G2 / G1

### Pin map (from the on-device label / schematic)

| Function                    | GPIO |
|-----------------------------|------|
| I2C SDA                     | G45  |
| I2C SCL                     | G0   |
| I2S MCLK                    | G11  |
| I2S SCLK / BCLK             | G17  |
| I2S LRCK / WS               | G3   |
| **DSDIN** (speaker, ESP→codec) | **G48** |
| **ASDOUT** (mic, codec→ESP) | **G4** |
| NS4150B PA enable           | G18  |
| Button (USER_BUT)           | G41  |

> ES8311 I2C address `0x18`. Note `DSDIN`/`ASDOUT`: the speaker data line is G48
> and the mic data line is G4 — getting these backwards gives white-noise output
> and a dead mic.
>
> There is **no programmable LED** on the VoiceS3R — the green LED is a hardware
> download-mode indicator driven by a separate PMS150G, not the ESP32.

## Boot flow

1. Bring up I2S audio + the ES8311 codec.
2. Load WiFi credentials from NVS; if present, connect.
3. If there are no credentials / the connection fails: play the "access point"
   prompt, raise the **`VoiceS3R`** SoftAP + a setup web page, and wait for the
   user to submit WiFi + the PC server address.
4. Play "ready for work", then run the assistant loop: on a wake word
   **"Sophia"** / **"Jarvis"** (or a button press) it beeps, records your command
   until you stop speaking, sends a 1-byte persona id (which name fired) + the
   PCM to the PC, and plays the spoken reply. The server can also set volume or
   enter WiFi speaker mode via a 1-byte control header.

## Provisioning

On first boot (or after a failed connection):

1. Connect a phone to WiFi **`VoiceS3R`** (password `21212122`).
2. Open **http://192.168.71.1/**.
3. Add up to **5 WiFi networks** (➕ Add WiFi), each with its SSID, password, and
   its own PC server `host:port` (e.g. `192.168.8.100:9000`) — handy when the
   server IP differs by location. Press **CONNECT**.

The device tries each network in order until one connects, and uses that
network's server address. Credentials persist in NVS across reboots. Speaker
volume is set by voice ("set volume 50"), not on the page.

## Building & flashing

Requires the Espressif Rust toolchain via [`espup`](https://github.com/esp-rs/espup):

```powershell
cargo install espup ldproxy espflash
espup install
. $HOME\export-esp.ps1   # sets LIBCLANG_PATH / PATH (run in every new shell)

cargo build --release
espflash flash --release --monitor   # or: cargo run --release
```

To enter **download mode**: hold the side reset button ~2 s until the green LED
lights, then release.

### Notes / gotchas

- **`.cargo/config.toml`** sets `target-dir = "C:/et"` — a Windows `MAX_PATH`
  workaround for ESP-IDF's deep build paths. Binaries land under `C:/et/...`.
- It also sets `ESP_IDF_SDKCONFIG_DEFAULTS` explicitly — without it, esp-idf-sys
  silently ignores `sdkconfig.defaults` (which sets the main-task stack, PSRAM,
  1 kHz tick, and a larger HTTP request-header limit). After changing
  `sdkconfig.defaults`, run `cargo clean -p esp-idf-sys` to force a reconfigure.

## Diagnostics

`src/control.rs` + `src/control.html` are a SoftAP web control panel used during
bring-up (test GPIO, beep, PA, volume, mic level, and ES8311 registers live
without reflashing). `src/bin/i2cscan.rs` is a standalone I2C scanner:

```powershell
cargo run --release --bin i2cscan
```

## Project layout

| File                | Role |
|---------------------|------|
| `src/main.rs`       | boot flow / orchestration |
| `src/wifi.rs`       | STA + SoftAP WiFi manager |
| `src/storage.rs`    | NVS-persisted config (SSID/pass/server/volume) |
| `src/provision.rs`  | SoftAP setup portal |
| `src/codec.rs`      | ES8311 init (I2C) + PA + registers |
| `src/audio.rs`      | full-duplex I2S, mono↔stereo, beep, mic level |
| `src/wakeword.rs`   | on-device dual "Sophia"+"Jarvis" wake word (ESP-SR / WakeNet9 AFE) |
| `src/net.rs`        | wake-word/button assistant loop + TCP streaming + speaker mode |
| `src/control.rs`    | diagnostic web panel (bring-up) |
| `assets/*.pcm`      | embedded 16 kHz TTS prompts (boot / setup) |

## License

MIT
