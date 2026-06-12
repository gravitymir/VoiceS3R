//! ES8311 audio codec control over I2C, plus the NS4150B power-amplifier enable.
//!
//! Pins (ATOM VoiceS3R): I2C SDA = G45, SCL = G4, PA enable = G3.

use anyhow::Result;
use es8311::{ClockConfig, Es8311, Resolution};
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::gpio::{Gpio0, Gpio18, Gpio45, Output, PinDriver};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_svc::hal::prelude::*;
use log::info;

use crate::config;

/// ES8311 default 7-bit I2C address (CE pin tied low).
const ES8311_ADDR: u8 = 0x18;

/// Holds the codec driver and the I2C bus it speaks on, so the bus stays alive.
pub struct Codec<'d> {
    i2c: I2cDriver<'d>,
    dev: Es8311,
    pa_enable: PinDriver<'d, Gpio18, Output>,
}

impl<'d> Codec<'d> {
    /// Initialise I2C, configure the ES8311 for 16 kHz / 16-bit mono with an
    /// external MCLK, and power on the speaker amplifier.
    /// Pins (ATOM VoiceS3R): SDA = G45, SCL = G0, NS4150_CTR (PA) = G18.
    pub fn new(
        i2c0: I2C0,
        sda: Gpio45,
        scl: Gpio0,
        pa: Gpio18,
    ) -> Result<Self> {
        let i2c_cfg = I2cConfig::new().baudrate(400.kHz().into());
        let mut i2c = I2cDriver::new(i2c0, sda, scl, &i2c_cfg)?;

        let mut delay = Delay::new_default();
        let dev = Es8311::new(ES8311_ADDR);

        // MCLK supplied on the dedicated pin (G11) at 256*fs. This is the config
        // the es8311 crate initialises cleanly with.
        let clock = ClockConfig {
            mclk_inverted: false,
            sclk_inverted: false,
            mclk_from_mclk_pin: true,
            mclk_frequency: config::MCLK_FREQ,
            sample_frequency: config::SAMPLE_RATE,
        };

        dev.init(
            &mut i2c,
            &clock,
            Resolution::Bits16,
            Resolution::Bits16,
            &mut delay,
        )
        .map_err(|e| anyhow::anyhow!("ES8311 init failed: {e:?}"))?;

        // Output (speaker) volume 0..=100.
        dev.volume_set(&mut i2c, 75, None)
            .map_err(|e| anyhow::anyhow!("ES8311 volume_set failed: {e:?}"))?;

        // Enable the NS4150B power amplifier.
        let pa_enable = PinDriver::output(pa)?;
        let mut codec = Self {
            i2c,
            dev,
            pa_enable,
        };
        codec.pa_enable.set_high()?;

        // Configure the microphone path (the crate's init() does NOT do this):
        // reg0x14 = enable analog MIC + PGA gain, reg0x17 = ADC gain full.
        codec.write_reg(0x14, 0x1A)?;
        codec.write_reg(0x17, 0xFF)?;

        info!("ES8311 codec initialised (16 kHz, MCLK-from-SCLK, mic on, PA on)");
        Ok(codec)
    }

    /// Set output volume (0..=100).
    pub fn set_volume(&mut self, vol: u8) -> Result<()> {
        self.dev
            .volume_set(&mut self.i2c, vol, None)
            .map_err(|e| anyhow::anyhow!("ES8311 volume_set failed: {e:?}"))?;
        Ok(())
    }

    /// Enable/disable the NS4150B power amplifier (G18).
    pub fn set_pa(&mut self, on: bool) -> Result<()> {
        if on {
            self.pa_enable.set_high()?;
        } else {
            self.pa_enable.set_low()?;
        }
        Ok(())
    }

    /// Read one ES8311 register over I2C.
    pub fn read_reg(&mut self, reg: u8) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(ES8311_ADDR, &[reg], &mut buf, 1000)?;
        Ok(buf[0])
    }

    /// Write one ES8311 register over I2C.
    pub fn write_reg(&mut self, reg: u8, val: u8) -> Result<()> {
        self.i2c.write(ES8311_ADDR, &[reg, val], 1000)?;
        Ok(())
    }
}
