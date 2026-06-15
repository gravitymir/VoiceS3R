//! WiFi provisioning portal: raise a SoftAP, serve a setup form (a list of up to
//! 5 WiFi networks + the PC server address), and return what the user submits.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use log::info;

use crate::config;
use crate::storage::{StoredConfig, WifiCred, MAX_WIFI};
use crate::wifi::WifiManager;

const PAGE: &str = r#"<!DOCTYPE html><html><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>ATOM VoiceS3R setup</title>
<style>
body{font-family:system-ui,sans-serif;max-width:440px;margin:16px auto;padding:0 16px;color:#222}
h2{color:#0a7}
label{display:block;margin:10px 0 3px;font-weight:600;font-size:14px}
input{width:100%;padding:9px;box-sizing:border-box;font-size:16px;border:1px solid #bbb;border-radius:5px}
.wblock{border:1px solid #ddd;border-radius:8px;padding:10px 12px;margin:12px 0;background:#fafafa}
.del{margin-top:8px;background:#c0392b;color:#fff;border:0;border-radius:5px;padding:7px 12px;font-size:14px}
#addbtn{background:#2a6;color:#fff;border:0;border-radius:6px;padding:9px 14px;font-size:15px;margin:4px 0 16px}
.connect{width:100%;padding:15px;font-size:18px;font-weight:700;background:#0a7;color:#fff;border:0;border-radius:8px;margin-top:18px}
#msg{margin-top:14px;font-size:15px;color:#0a7;min-height:20px}
</style></head><body>
<h2>ATOM VoiceS3R setup</h2>
<div id=wifis></div>
<button type=button id=addbtn onclick="addWifi()">+ Add WiFi</button>
<button type=button class=connect onclick="save()">CONNECT</button>
<p id=msg></p>
<script>
var MAX=__MAX__;
var DEF_SRV='__SERVER__';
var DEF_SSID='__SSID__';
var nets=[{ssid:DEF_SSID,pass:'',server:DEF_SRV}];
function esc(s){return (s||'').replace(/"/g,'&quot;');}
function render(){
  var c=document.getElementById('wifis');c.innerHTML='';
  nets.forEach(function(n,i){
    var d=document.createElement('div');d.className='wblock';
    d.innerHTML='<label>WiFi '+(i+1)+' (SSID)</label>'+
      '<input data-i="'+i+'" data-k="ssid" placeholder="network name" value="'+esc(n.ssid)+'">'+
      '<label>Password</label>'+
      '<input data-i="'+i+'" data-k="pass" type=password placeholder="leave empty if open" value="'+esc(n.pass)+'">'+
      '<label>PC server (host:port)</label>'+
      '<input data-i="'+i+'" data-k="server" placeholder="192.168.x.x:9000" value="'+esc(n.server)+'">'+
      (i>0?'<button type=button class=del onclick="delWifi('+i+')">delete</button>':'');
    c.appendChild(d);
  });
  c.querySelectorAll('input').forEach(function(inp){
    inp.oninput=function(){nets[inp.dataset.i][inp.dataset.k]=inp.value;};
  });
  document.getElementById('addbtn').style.display=(nets.length>=MAX)?'none':'';
}
function addWifi(){if(nets.length<MAX){nets.push({ssid:'',pass:'',server:DEF_SRV});render();}}
function delWifi(i){if(i>0){nets.splice(i,1);render();}}
function save(){
  var body=new URLSearchParams();var count=0;
  nets.forEach(function(n){
    if((n.ssid||'').trim()!==''){
      body.append('ssid'+count,n.ssid);
      body.append('pass'+count,n.pass);
      body.append('srv'+count,n.server||DEF_SRV);
      count++;
    }
  });
  if(count===0){document.getElementById('msg').textContent='Add at least one WiFi network.';return;}
  document.getElementById('msg').textContent='S3R is turning off this setup network and will try to connect to your WiFi list...';
  fetch('/connect',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:body.toString()})
    .then(function(r){return r.text();})
    .then(function(t){document.getElementById('msg').textContent=t;})
    .catch(function(){document.getElementById('msg').textContent='Saved. S3R is turning off the setup network and trying your WiFi list.';});
}
render();
</script></body></html>"#;

const DONE: &str =
    "Saved. S3R is turning off the setup network and connecting to your WiFi list. You can close this page.";

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
            .replace("__SSID__", config::DEFAULT_SSID)
            .replace("__MAX__", &MAX_WIFI.to_string());
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
            if n == 0 || body.len() > 4096 {
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

    loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(cfg) = result.lock().unwrap().take() {
            info!("received {} network(s)", cfg.wifis.len());
            return Ok(cfg);
        }
    }
}

/// Parse `application/x-www-form-urlencoded` body into a config.
fn parse_form(body: &[u8]) -> Option<StoredConfig> {
    let s = String::from_utf8_lossy(body);
    let pairs: Vec<(String, String)> = s
        .split('&')
        .map(|p| {
            let mut it = p.splitn(2, '=');
            (
                it.next().unwrap_or("").to_string(),
                url_decode(it.next().unwrap_or("")),
            )
        })
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    let mut wifis = Vec::new();
    for i in 0..MAX_WIFI {
        if let Some(ssid) = get(&format!("ssid{i}")) {
            if !ssid.is_empty() {
                let pass = get(&format!("pass{i}")).unwrap_or_default();
                let server = get(&format!("srv{i}"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| config::DEFAULT_SERVER.to_string());
                wifis.push(WifiCred { ssid, pass, server });
            }
        }
    }
    if wifis.is_empty() {
        return None;
    }

    // Volume is voice-controlled now; default it (changed at runtime via "set volume").
    Some(StoredConfig {
        wifis,
        volume: config::DEFAULT_VOLUME,
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
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
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
