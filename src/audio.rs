//! Full-duplex I2S audio for the ES8311.
//!
//! Pins (ATOM VoiceS3R on-device label): MCLK = G11, SCLK/BCLK = G17, LRCK/WS = G3,
//! DSDIN = G48 (codec speaker input = ESP data OUT),
//! ASDOUT = G4 (codec mic output = ESP data IN).
//!
//! Runs the I2S bus in STEREO 16-bit (matching M5's working config — the ES8311
//! needs the standard 2-slot frame even though it is a mono codec). MCLK is
//! derived by the codec from BCLK, so no MCLK pin is driven.
//!
//! The public helpers work in MONO 16-bit PCM and convert to/from the stereo
//! bus internally (duplicate to L+R on playback, take left on capture).

use anyhow::Result;
use esp_idf_svc::hal::delay::BLOCK;
use esp_idf_svc::hal::gpio::{Gpio11, Gpio17, Gpio3, Gpio4, Gpio48};
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sBiDir, I2sDriver, I2S0};

use crate::config;

/// Stereo scratch buffer is twice the mono chunk size.
const STEREO_BYTES: usize = config::AUDIO_CHUNK_BYTES * 2;

pub struct Audio<'d> {
    drv: I2sDriver<'d, I2sBiDir>,
}

impl<'d> Audio<'d> {
    pub fn new(
        i2s0: I2S0,
        bclk: Gpio17,
        ws: Gpio3,
        mclk: Gpio11,
        din: Gpio4,   // ASDOUT: mic data from codec into ESP
        dout: Gpio48, // DSDIN: speaker data from ESP into codec
    ) -> Result<Self> {
        let clk = StdClkConfig::from_sample_rate_hz(config::SAMPLE_RATE);
        let slot = StdSlotConfig::philips_slot_default(DataBitWidth::Bits16, SlotMode::Stereo);
        // auto_clear: on TX underrun, output silence instead of looping the last
        // DMA buffer (otherwise playback leaves a repeating noise tail).
        let cfg = StdConfig::new(
            Config::new().auto_clear(true),
            clk,
            slot,
            StdGpioConfig::default(),
        );

        let mut drv = I2sDriver::new_std_bidir(
            i2s0,
            &cfg,
            bclk,
            din,
            dout,
            Some(mclk),
            ws,
        )?;
        drv.rx_enable()?;
        drv.tx_enable()?;

        Ok(Self { drv })
    }

    /// Raw stereo write to the I2S bus.
    fn write_raw(&mut self, buf: &[u8]) -> Result<usize> {
        Ok(self.drv.write(buf, BLOCK)?)
    }

    fn write_all_raw(&mut self, buf: &[u8]) -> Result<()> {
        let mut written = 0;
        while written < buf.len() {
            written += self.write_raw(&buf[written..])?;
        }
        Ok(())
    }

    /// Push a stereo chunk of silence to settle the DAC after playback.
    pub fn write_silence(&mut self) -> Result<()> {
        let silence = [0u8; STEREO_BYTES];
        self.write_all_raw(&silence)?;
        Ok(())
    }

    /// Play a buffer of 16 kHz/16-bit *mono* PCM (duplicated to L+R).
    pub fn play_pcm(&mut self, mono: &[u8]) -> Result<()> {
        let mut stereo = [0u8; STEREO_BYTES];
        for chunk in mono.chunks(config::AUDIO_CHUNK_BYTES) {
            let mut si = 0;
            for s in chunk.chunks_exact(2) {
                stereo[si] = s[0];
                stereo[si + 1] = s[1];
                stereo[si + 2] = s[0];
                stereo[si + 3] = s[1];
                si += 4;
            }
            self.write_all_raw(&stereo[..si])?;
        }
        self.write_silence().ok();
        Ok(())
    }

    /// Play a square-wave beep at `freq_hz` for `ms` (stereo).
    pub fn beep(&mut self, freq_hz: u32, ms: u32) -> Result<()> {
        let total = (config::SAMPLE_RATE as u64 * ms as u64 / 1000) as usize;
        let half_period = (config::SAMPLE_RATE / freq_hz.max(1) / 2).max(1) as usize;
        const AMP: i16 = 7000;
        let mut buf = [0u8; STEREO_BYTES];
        let mut produced = 0usize;
        let mut idx = 0usize;
        while produced < total {
            let mut bi = 0;
            while bi + 3 < buf.len() && produced < total {
                let s = if (idx / half_period) % 2 == 0 { AMP } else { -AMP };
                let le = s.to_le_bytes();
                buf[bi] = le[0];
                buf[bi + 1] = le[1];
                buf[bi + 2] = le[0];
                buf[bi + 3] = le[1];
                bi += 4;
                idx += 1;
                produced += 1;
            }
            self.write_all_raw(&buf[..bi])?;
        }
        self.write_silence().ok();
        Ok(())
    }

    /// Read a burst of mic audio and return the peak |sample| (left channel).
    pub fn mic_peak(&mut self, frames: usize) -> Result<i32> {
        let mut buf = [0u8; STEREO_BYTES];
        let mut peak = 0i32;
        for _ in 0..frames {
            let n = self.drv.read(&mut buf, BLOCK)?;
            for st in buf[..n].chunks_exact(4) {
                let l = i16::from_le_bytes([st[0], st[1]]) as i32;
                peak = peak.max(l.abs());
            }
        }
        Ok(peak)
    }

    /// Read mic audio as MONO 16-bit PCM (left channel of the stereo bus).
    /// `out` receives up to `out.len()` bytes; returns bytes written.
    pub fn read_mono(&mut self, out: &mut [u8]) -> Result<usize> {
        let mut st = [0u8; STEREO_BYTES];
        let want = (out.len() * 2).min(STEREO_BYTES);
        let n = self.drv.read(&mut st[..want], BLOCK)?;
        let mut mi = 0;
        for frame in st[..n].chunks_exact(4) {
            out[mi] = frame[0];
            out[mi + 1] = frame[1];
            mi += 2;
        }
        Ok(mi)
    }

    /// Fill `out` with mono 16-bit mic samples (blocks until full). Used to feed
    /// fixed-size frames to the wake-word AFE.
    pub fn read_samples(&mut self, out: &mut [i16]) -> Result<()> {
        let mut byte_buf = [0u8; config::AUDIO_CHUNK_BYTES];
        let mut done = 0;
        while done < out.len() {
            let n = self.read_mono(&mut byte_buf)? / 2;
            let take = n.min(out.len() - done);
            for i in 0..take {
                out[done + i] = i16::from_le_bytes([byte_buf[i * 2], byte_buf[i * 2 + 1]]);
            }
            done += take;
        }
        Ok(())
    }

    /// Write MONO 16-bit PCM to the speaker (duplicated to L+R), no trailing silence.
    pub fn write_mono(&mut self, mono: &[u8]) -> Result<()> {
        let mut st = [0u8; STEREO_BYTES];
        for chunk in mono.chunks(config::AUDIO_CHUNK_BYTES) {
            let mut si = 0;
            for s in chunk.chunks_exact(2) {
                st[si] = s[0];
                st[si + 1] = s[1];
                st[si + 2] = s[0];
                st[si + 3] = s[1];
                si += 4;
            }
            self.write_all_raw(&st[..si])?;
        }
        Ok(())
    }
}
