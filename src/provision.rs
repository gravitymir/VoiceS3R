//! WiFi provisioning portal: raise a SoftAP, serve a setup form, and return the
//! credentials the user submits from their phone.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use log::info;

use crate::config;
use crate::storage::StoredConfig;
use crate::wifi::WifiManager;

const PAGE: &str = r#"<!DOCTYPE html><html><head><meta name=viewport content="width=device-width,initial-scale=1">
<title>ATOM VoiceS3R setup</title>
<style>body{font-family:sans-serif;max-width:420px;margin:24px auto;padding:0 16px}
label{display:block;margin:12px 0 4px;font-weight:600}
input{width:100%;padding:10px;box-sizing:border-box;font-size:16px}
button{margin-top:20px;width:100%;padding:12px;font-size:16px;background:#0a7;color:#fff;border:0;border-radius:6px}
h2{color:#0a7}</style></head><body>
<h2>ATOM VoiceS3R setup</h2>
<form id=f onsubmit="return false">
<label>WiFi network (SSID)</label><input name=ssid placeholder="your-wifi" required>
<label>WiFi password</label><input name=pass type=password placeholder="leave empty if open">
<label>PC server (host:port)</label><input name=server value="__SERVER__">
<label>Speaker volume (0-100)</label><input name=volume type=number min=0 max=100 value="__VOL__">
<button type=submit onclick="save()">CONNECT</button>
</form>
<p id=msg></p>
<script>
function save(){
  var f=document.getElementById('f');
  if(!f.ssid.value){f.ssid.focus();return;}
  var body=new URLSearchParams(new FormData(f)).toString();
  document.getElementById('msg').textContent='Saving and connecting...';
  fetch('/connect',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:body})
    .then(function(r){return r.text();})
    .then(function(t){document.getElementById('msg').textContent=t;})
    .catch(function(){document.getElementById('msg').textContent='Saved. Device is connecting to WiFi — this setup network will turn off.';});
}
</script></body></html>"#;

const DONE: &str = "Saved. Connecting to WiFi — this setup network will turn off. You can close this page.";

/// Run the portal until the user submits credentials, then return them.
pub fn run_portal(wifi: &mut WifiManager) -> Result<StoredConfig> {
    let ip = wifi.start_ap(config::AP_SSID, config::AP_PASS)?;
    info!(
        "Provisioning AP up: SSID '{}', pass '{}'. Browse to http://{}/",
        config::AP_SSID,
        config::AP_PASS,
        ip
    );

    let result: Arc<Mutex<Option<StoredConfig>>> = Arc::new(Mutex::new(None));

    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    server.fn_handler::<anyhow::Error, _>("/", Method::Get, |req| {
        let html = PAGE
            .replace("__SERVER__", config::DEFAULT_SERVER)
            .replace("__VOL__", &config::DEFAULT_VOLUME.to_string());
        let mut resp = req.into_ok_response()?;
        resp.write_all(html.as_bytes())?;
        Ok(())
    })?;

    let sink = result.clone();
    server.fn_handler::<anyhow::Error, _>("/connect", Method::Post, move |mut req| {
        let mut body = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = req.read(&mut buf)?;
            if n == 0 || body.len() > 2048 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        let cfg = parse_form(&body);
        let mut resp = req.into_ok_response()?;
        resp.write_all(DONE.as_bytes())?;
        if let Some(cfg) = cfg {
            *sink.lock().unwrap() = Some(cfg);
        }
        Ok(())
    })?;

    // Block until a submission arrives; dropping `server` stops it.
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(cfg) = result.lock().unwrap().take() {
            info!("received credentials for '{}'", cfg.ssid);
            return Ok(cfg);
        }
    }
}

/// Parse `application/x-www-form-urlencoded` body into a config.
fn parse_form(body: &[u8]) -> Option<StoredConfig> {
    let s = String::from_utf8_lossy(body);
    let mut ssid = String::new();
    let mut pass = String::new();
    let mut server = config::DEFAULT_SERVER.to_string();
    let mut volume = config::DEFAULT_VOLUME;

    for pair in s.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let val = url_decode(it.next().unwrap_or(""));
        match key {
            "ssid" => ssid = val,
            "pass" => pass = val,
            "server" if !val.is_empty() => server = val,
            "volume" => volume = val.parse().unwrap_or(config::DEFAULT_VOLUME).min(100),
            _ => {}
        }
    }

    if ssid.is_empty() {
        return None;
    }
    Some(StoredConfig {
        ssid,
        pass,
        server,
        volume,
    })
}

/// Minimal percent-decoding for form values (`+` -> space, `%XX` -> byte).
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(h << 4 | l);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
