//! Diagnostic: scan I2C for the ES8311 on candidate pin pairs.
//! Run with `cargo run --release --bin i2cscan` and read the serial log.

use esp_idf_svc::hal::gpio::{AnyIOPin, IOPin};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::hal::prelude::*;
use log::info;

fn scan(name: &str, i2c: esp_idf_svc::hal::i2c::I2C0, sda: AnyIOPin, scl: AnyIOPin) {
    let cfg = I2cConfig::new().baudrate(100.kHz().into());
    let mut drv = match I2cDriver::new(i2c, sda, scl, &cfg) {
        Ok(d) => d,
        Err(e) => {
            info!("[{name}] driver init failed: {e:?}");
            return;
        }
    };
    info!("[{name}] scanning 0x08..0x77 ...");
    let mut found = 0;
    for addr in 0x08u8..=0x77 {
        // A one-byte write that ACKs means a device is present. Use a finite
        // timeout (50 ms) so a non-responding bus moves on instead of blocking.
        if drv.write(addr, &[0x00], 50).is_ok() {
            info!("[{name}] ACK at 0x{addr:02X}");
            found += 1;
        }
    }
    info!("[{name}] done, {found} device(s)");
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("I2C scanner starting");

    let p = Peripherals::take().unwrap();
    let mut i2c = p.i2c0;
    let pins = p.pins;

    // Candidate B: Echo Base style (SDA G38 / SCL G39).
    scan("B 38/39", i2c, pins.gpio38.downgrade(), pins.gpio39.downgrade());

    info!("I2C scan complete. Reset to re-run.");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
}
