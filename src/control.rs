//! Diagnostic control panel served over the SoftAP. Lets us test hardware
//! (GPIO, beep, PA, volume, mic, ES8311 registers) live from a phone browser,
//! without reflashing between experiments.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use log::info;

use crate::audio::Audio;
use crate::codec::Codec;
use crate::config;

pub type SharedAudio = Arc<Mutex<Option<Audio<'static>>>>;
pub type SharedCodec = Arc<Mutex<Option<Codec<'static>>>>;

const PAGE: &str = include_str!("control.html");

/// Start the control server and block forever.
pub fn run(ap_ip: Ipv4Addr, audio: SharedAudio, codec: SharedCodec) -> Result<()> {
    info!("control panel at http://{ap_ip}/");
    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    server.fn_handler::<anyhow::Error, _>("/", Method::Get, |req| {
        let mut r = req.into_ok_response()?;
        r.write_all(PAGE.as_bytes())?;
        Ok(())
    })?;

    // ---- /info : status text ----
    {
        let audio = audio.clone();
        let codec = codec.clone();
        server.fn_handler::<anyhow::Error, _>("/info", Method::Get, move |req| {
            let audio_ok = audio.lock().unwrap().is_some();
            let codec_ok = codec.lock().unwrap().is_some();
            let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            let body = format!(
                "chip=ESP32-S3-PICO  ip={ap_ip}\naudio_init={audio_ok}  codec_init={codec_ok}\nfree_heap={heap} bytes\nAP SSID={} pass={}",
                config::AP_SSID, config::AP_PASS
            );
            let mut r = req.into_ok_response()?;
            r.write_all(body.as_bytes())?;
            Ok(())
        })?;
    }

    // ---- /beep?freq=&ms= ----
    {
        let audio = audio.clone();
        server.fn_handler::<anyhow::Error, _>("/beep", Method::Get, move |req| {
            let uri = req.uri().to_string();
            let freq = qparam(&uri, "freq").and_then(|v| v.parse().ok()).unwrap_or(880u32);
            let ms = qparam(&uri, "ms").and_then(|v| v.parse().ok()).unwrap_or(200u32);
            let msg = match audio.lock().unwrap().as_mut() {
                Some(a) => match a.beep(freq, ms) {
                    Ok(()) => format!("beep {freq}Hz {ms}ms OK"),
                    Err(e) => format!("beep error: {e:?}"),
                },
                None => "audio not initialized".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /play : play the embedded "ready" prompt ----
    {
        let audio = audio.clone();
        server.fn_handler::<anyhow::Error, _>("/play", Method::Get, move |req| {
            let msg = match audio.lock().unwrap().as_mut() {
                Some(a) => match a.play_pcm(config::PROMPT_READY) {
                    Ok(()) => "played ready prompt".to_string(),
                    Err(e) => format!("play error: {e:?}"),
                },
                None => "audio not initialized".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /mic : capture and report peak level ----
    {
        let audio = audio.clone();
        server.fn_handler::<anyhow::Error, _>("/mic", Method::Get, move |req| {
            let msg = match audio.lock().unwrap().as_mut() {
                Some(a) => match a.mic_peak(16) {
                    Ok(p) => format!("mic peak = {p} / 32767"),
                    Err(e) => format!("mic error: {e:?}"),
                },
                None => "audio not initialized".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /pa?on=1 : toggle power amplifier ----
    {
        let codec = codec.clone();
        server.fn_handler::<anyhow::Error, _>("/pa", Method::Get, move |req| {
            let uri = req.uri().to_string();
            let on = qparam(&uri, "on").map(|v| v == "1").unwrap_or(true);
            let msg = match codec.lock().unwrap().as_mut() {
                Some(c) => match c.set_pa(on) {
                    Ok(()) => format!("PA (G18) = {}", if on { "ON" } else { "OFF" }),
                    Err(e) => format!("pa error: {e:?}"),
                },
                None => "codec not initialized".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /vol?v=NN ----
    {
        let codec = codec.clone();
        server.fn_handler::<anyhow::Error, _>("/vol", Method::Get, move |req| {
            let uri = req.uri().to_string();
            let v: u8 = qparam(&uri, "v").and_then(|s| s.parse().ok()).unwrap_or(75).min(100);
            let msg = match codec.lock().unwrap().as_mut() {
                Some(c) => match c.set_volume(v) {
                    Ok(()) => format!("volume = {v}"),
                    Err(e) => format!("vol error: {e:?}"),
                },
                None => "codec not initialized".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /reg?addr=NN (read) and optional &val=NN (write) ----
    {
        let codec = codec.clone();
        server.fn_handler::<anyhow::Error, _>("/reg", Method::Get, move |req| {
            let uri = req.uri().to_string();
            let addr = qparam(&uri, "addr").and_then(|s| parse_u8(&s));
            let val = qparam(&uri, "val").and_then(|s| parse_u8(&s));
            let msg = match (codec.lock().unwrap().as_mut(), addr) {
                (Some(c), Some(a)) => {
                    if let Some(v) = val {
                        match c.write_reg(a, v) {
                            Ok(()) => format!("wrote reg 0x{a:02X} = 0x{v:02X}"),
                            Err(e) => format!("regw error: {e:?}"),
                        }
                    } else {
                        match c.read_reg(a) {
                            Ok(v) => format!("reg 0x{a:02X} = 0x{v:02X} ({v})"),
                            Err(e) => format!("regr error: {e:?}"),
                        }
                    }
                }
                (None, _) => "codec not initialized".to_string(),
                (_, None) => "missing addr".to_string(),
            };
            reply(req, &msg)
        })?;
    }

    // ---- /gpio?pin=N&level=L : raw drive any GPIO (LED hunting) ----
    server.fn_handler::<anyhow::Error, _>("/gpio", Method::Get, move |req| {
        let uri = req.uri().to_string();
        let pin = qparam(&uri, "pin").and_then(|s| s.parse::<i32>().ok());
        let level = qparam(&uri, "level").and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
        let msg = match pin {
            Some(p) => {
                unsafe {
                    esp_idf_svc::sys::gpio_reset_pin(p);
                    esp_idf_svc::sys::gpio_set_direction(
                        p,
                        esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_OUTPUT,
                    );
                    esp_idf_svc::sys::gpio_set_level(p, level);
                }
                format!("GPIO{p} set to {level}")
            }
            None => "missing pin".to_string(),
        };
        reply(req, &msg)
    })?;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn reply(
    req: esp_idf_svc::http::server::Request<&mut esp_idf_svc::http::server::EspHttpConnection<'_>>,
    msg: &str,
) -> Result<()> {
    let mut r = req.into_ok_response()?;
    r.write_all(msg.as_bytes())?;
    Ok(())
}

fn qparam(uri: &str, key: &str) -> Option<String> {
    let q = uri.split('?').nth(1)?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next()? == key {
            return it.next().map(|s| s.to_string());
        }
    }
    None
}

/// Parse a u8 in decimal or `0x` hex.
fn parse_u8(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}
