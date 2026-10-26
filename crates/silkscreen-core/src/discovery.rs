//! LAN discovery over mDNS/DNS-SD. Displays advertise themselves (including
//! their public key, so hosts can tell paired displays apart before
//! connecting); hosts browse for them.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use silkscreen_proto::{MDNS_SERVICE_TYPE, PROTOCOL_VERSION};

/// Keeps a display advertised until dropped.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub touch: bool,
}

pub fn advertise(name: &str, port: u16, key: &[u8; 32], s: &Summary) -> Result<Advertiser> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    // A valid, unique DNS label (device names may contain spaces, and
    // Android's hostname is just "localhost").
    let host = format!("silkscreen-{}", &hex::encode(key)[..12]);
    let props = [
        ("name", name.to_string()),
        ("key", hex::encode(key)),
        ("w", s.width.to_string()),
        ("h", s.height.to_string()),
        ("mhz", s.refresh_mhz.to_string()),
        ("touch", (s.touch as u8).to_string()),
        ("proto", PROTOCOL_VERSION.to_string()),
        ("ver", env!("CARGO_PKG_VERSION").to_string()),
    ];
    // The instance name must be unique on the LAN; the key prefix makes it so
    // even when two devices share a display name.
    let instance = format!("{name} [{}]", &hex::encode(key)[..8]);
    let info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance,
        &format!("{host}.local."),
        "",
        port,
        &props[..],
    )?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_owned();
    daemon.register(info)?;
    Ok(Advertiser { daemon, fullname })
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FoundDisplay {
    pub name: String,
    pub addr: SocketAddr,
    /// The display's public key, if it advertised one.
    pub key: Option<[u8; 32]>,
    pub summary: Summary,
    pub protocol: u16,
    /// The display's Silkscreen version ("0.2.1"); empty before 0.2.1.
    pub version: String,
}

fn parse(svc: &ResolvedService) -> Option<FoundDisplay> {
    // Prefer IPv4: it is what the video socket binds to.
    let ip = svc.get_addresses_v4().into_iter().min()?;
    let prop = |k: &str| svc.get_property_val_str(k).unwrap_or_default().to_string();
    let num = |k: &str| prop(k).parse::<u32>().unwrap_or(0);
    let name = match prop("name") {
        n if n.is_empty() => svc.get_fullname().to_string(),
        n => n,
    };
    Some(FoundDisplay {
        name,
        addr: SocketAddr::new(IpAddr::V4(ip), svc.get_port()),
        key: hex::decode(prop("key"))
            .ok()
            .and_then(|k| k.try_into().ok()),
        summary: Summary {
            width: num("w"),
            height: num("h"),
            refresh_mhz: num("mhz"),
            touch: num("touch") != 0,
        },
        protocol: num("proto") as u16,
        version: prop("ver"),
    })
}

/// Collects displays seen on the LAN within `timeout`.
pub fn browse(timeout: Duration) -> Result<Vec<FoundDisplay>> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<FoundDisplay> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(ev) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(svc) = ev
            && let Some(d) = parse(&svc)
            && !found.iter().any(|f| f.addr == d.addr)
        {
            found.push(d);
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

/// Continuously tracks displays on the LAN (for the GUI).
pub struct Browser {
    found: Arc<Mutex<HashMap<String, FoundDisplay>>>,
    stop: Arc<AtomicBool>,
}

impl Browser {
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
        let found = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (found, stop) = (found.clone(), stop.clone());
            std::thread::Builder::new()
                .name("mdns-browse".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(ev) = rx.recv_timeout(Duration::from_millis(250)) else {
                            continue;
                        };
                        let mut f = found.lock().unwrap();
                        let changed = match ev {
                            ServiceEvent::ServiceResolved(svc) => match parse(&svc) {
                                Some(d) => {
                                    f.insert(svc.get_fullname().to_owned(), d.clone()) != Some(d)
                                }
                                None => false,
                            },
                            ServiceEvent::ServiceRemoved(_, fullname) => {
                                f.remove(&fullname).is_some()
                            }
                            _ => false,
                        };
                        drop(f);
                        if changed {
                            crate::ctx::wake_ui();
                        }
                    }
                    let _ = daemon.shutdown();
                })?;
        }
        Ok(Browser { found, stop })
    }

    /// Current displays, sorted by name.
    pub fn displays(&self) -> Vec<FoundDisplay> {
        let mut v: Vec<_> = self.found.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.addr.cmp(&b.addr))
        });
        v
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
