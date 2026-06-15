//! Compile-time constants and embedded assets. Runtime WiFi/server settings now
//! live in NVS (see `storage.rs`) and are set via the provisioning portal.

/// SoftAP SSID raised when the device has no working WiFi config.
pub const AP_SSID: &str = "VoiceS3R";
/// SoftAP password (WPA2, must be >= 8 chars).
pub const AP_PASS: &str = "21212122";

/// Default SSID pre-filled in the setup form's first WiFi block.
pub const DEFAULT_SSID: &str = "home";
/// Default value pre-filled in the setup form for the PC server `host:port`.
pub const DEFAULT_SERVER: &str = "192.168.8.100:9000";
/// Default speaker volume (0..=100).
pub const DEFAULT_VOLUME: u8 = 75;

/// Audio sample rate (mono, 16-bit PCM). Whisper expects 16 kHz.
pub const SAMPLE_RATE: u32 = 16_000;
/// MCLK fed to the ES8311. 256 * sample_rate is a safe ratio for 16-bit.
pub const MCLK_FREQ: u32 = SAMPLE_RATE * 256;
/// Playback/record DMA chunk size in bytes (i16 samples * 2). ~32 ms at 16 kHz.
pub const AUDIO_CHUNK_BYTES: usize = 1024;

/// Spoken prompt played when entering provisioning (AP) mode. 16 kHz mono PCM.
pub const PROMPT_AP: &[u8] = include_bytes!("../assets/prompt_ap.pcm");
/// Spoken prompt played once WiFi connects. 16 kHz mono PCM.
pub const PROMPT_READY: &[u8] = include_bytes!("../assets/prompt_ready.pcm");
