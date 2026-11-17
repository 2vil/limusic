//! The embedded Cloudflare WARP tunnel (the Settings "experimental" toggle).
//!
//! YT Music is not served in every country; users there need their traffic to come from an IP in a
//! served one. The app already routes everything through the `proxy` setting (#241), so what is
//! missing is a provider: this module embeds a Cloudflare WARP WireGuard client, exposes it as a
//! local no-auth SOCKS5 server, and points the app's proxy plumbing at it. No root, no TUN
//! device, no external binary — one toggle, on desktop and Android alike.
//!
//! Three pieces, in dependency order:
//! 1. **Registration** ([`register`]): one anonymous device against Cloudflare's client API, the
//!    way `wgcf` does it. The device blob (private key included) is stored in the database like
//!    other session material — it never crosses into the webview, so it must not join
//!    `UI_SETTINGS`.
//! 2. **The engine** ([`start_tunnel`]): the device is turned into a stock WireGuard `.conf`
//!    (the format `wgcf` writes) and handed to `wgproxyrs` — a userspace WireGuard engine
//!    (boringtun + smoltcp) that keeps the session alive and resolves domain targets through the
//!    tunnel's own DNS. Chosen after the first engine this module carried (`tokio-wireguard`)
//!    dropped its session mid-flight on CGNAT-flaky networks no matter how it was wired; the
//!    same probe battery that killed it runs clean on `wgproxyrs` for as long as it was run.
//! 3. **The fronts**: `wgproxyrs` serves SOCKS5 (`run_socks5_listener`) and HTTP CONNECT
//!    (`run_http_listener`) over one engine — mpv needs the latter, because libavformat only
//!    speaks `http://` proxies (see `player::set_http_proxy`).
//!
//! DNS resolves through the tunnel (the generated `.conf` pins `DNS = 1.1.1.1`, and the engine
//! carries a smoltcp DNS socket), so domain targets are answered inside the tunnel rather than
//! leaked to the local resolver.
//!
//! State machine: `Off → Registering → Connecting → Connected`, any step failing → `Failed(msg)`
//! with the proxy untouched. Toggling off tears the engine down and restores the user's manual
//! `proxy` setting.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;
use tokio::sync::watch;
use wgproxyrs::config::Config;
use wgproxyrs::keys::generate_keypair;
use wgproxyrs::proxy::{run_http_listener, run_socks5_listener, ProxyCtx};
use wgproxyrs::stack::{spawn_engine, EngineConfig, EngineHandle, PeerTunnel};
use wgproxyrs::tunnel::{UdpTransport, WgTunnel};

use crate::db::Db;

/// Base ports for the two local front ends. Picked once so the webview proxy URL is stable across
/// launches; on a collision the next few ports are tried before giving up.
const SOCKS_PORT_BASE: u16 = 51879;
const HTTP_PORT_BASE: u16 = 51890;
/// How many ports past each base [`start_tunnel`] will try before failing.
const PORT_ATTEMPTS: u16 = 5;
/// WARP peers forward everything, so the tunnel's routing table is "the world".
const ALL_V4: &str = "0.0.0.0/0";
/// Aggressive on purpose: the first engine lost its session on networks whose NAT forgets UDP
/// flows faster than the `wg`-default 25 s, and 10 s costs nothing.
const KEEPALIVE_SECS: u16 = 10;

/// Cloudflare's client registration API, the same versioned surface `wgcf` targets. A plain
/// Android app user agent is what their intake expects.
const REG_URL: &str = "https://api.cloudflareclient.com/v0a2158/reg";
const REG_UA: &str = "okhttp/3.12.1";

/// What the UI is told, through the `warp-status` event and the `warp_status` command. Serialized
/// with the tag on the outside so JS sees `{ status: "connected" }` / `{ status: "failed", msg }`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WarpStatus {
    Off,
    Registering,
    Connecting,
    Connected,
    Failed { msg: String },
}

/// The one thing the database must keep for a device: enough to rebuild the tunnel without
/// talking to the registration API again. Flattened from the API's nested response at
/// registration time, so schema drift on their side can only fail once, here, with a clear error.
#[derive(Clone)]
struct WarpDevice {
    private_key_b64: String,
    v4_address: String,
    peer_public_key_b64: String,
    endpoint: String,
}

impl WarpDevice {
    fn to_json(&self) -> String {
        serde_json::json!({
            "private_key": self.private_key_b64,
            "v4": self.v4_address,
            "peer_public_key": self.peer_public_key_b64,
            "endpoint": self.endpoint,
        })
        .to_string()
    }

    fn from_json(json: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        Some(Self {
            private_key_b64: v.get("private_key")?.as_str()?.to_owned(),
            v4_address: v.get("v4")?.as_str()?.to_owned(),
            peer_public_key_b64: v.get("peer_public_key")?.as_str()?.to_owned(),
            endpoint: v.get("endpoint")?.as_str()?.to_owned(),
        })
    }

    /// The device as a stock WireGuard `.conf`, the one shape [`Config::parse`] takes. `DNS` is
    /// pinned to 1.1.1.1 because the engine resolves domain targets through the tunnel (no leak
    /// to the local resolver) and the registration reply carries no DNS line of its own. The v4
    /// address arrives bare (`172.16.0.2`) and needs the `/32` that `.conf` files carry.
    fn wg_conf(&self) -> String {
        let v4 = if self.v4_address.contains('/') {
            self.v4_address.clone()
        } else {
            format!("{}/32", self.v4_address)
        };
        format!(
            "[Interface]\nPrivateKey = {key}\nAddress = {v4}\nDNS = 1.1.1.1\n\n[Peer]\n\
             PublicKey = {peer}\nAllowedIPs = {all}\nEndpoint = {endpoint}\n\
             PersistentKeepalive = {keep}\n",
            key = self.private_key_b64,
            peer = self.peer_public_key_b64,
            all = ALL_V4,
            endpoint = self.endpoint,
            keep = KEEPALIVE_SECS,
        )
    }
}

pub struct WarpManager {
    app: AppHandle,
    status: RwLock<WarpStatus>,
    tunnel: Mutex<Option<Tunnel>>,
}

struct Tunnel {
    engine: EngineHandle,
    /// The two front-end loops; aborted with the engine on shutdown.
    front_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Retires this tunnel's watchdog when the tunnel goes away. Recreated on every rebuild, so
    /// exactly one watchdog watches exactly one tunnel.
    shutdown: watch::Sender<bool>,
    socks_port: u16,
    http_port: u16,
}

impl Tunnel {
    fn shutdown_rx(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    fn kill(self) {
        let _ = self.shutdown.send(true);
        self.engine.stop();
        for task in self.front_tasks {
            task.abort();
        }
    }
}

impl WarpManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self { app, status: RwLock::new(WarpStatus::Off), tunnel: Mutex::new(None) })
    }

    pub fn status(&self) -> WarpStatus {
        self.status.read().unwrap().clone()
    }

    fn set_status(&self, status: WarpStatus) {
        *self.status.write().unwrap() = status;
        // The SPA mirrors the toggle's state live (Settings ▸ General): one event, and its payload
        // is the same shape the `warp_status` command returns.
        let current = self.status.read().unwrap().clone();
        let _ = self.app.emit("warp-status", &current);
    }

    /// The commands' entry point. Starting while connected is a no-op (the UI's switch already
    /// reflects reality), stopping while off too.
    pub async fn set_enabled(&self, db: &Db, on: bool) -> Result<(), String> {
        if on {
            if self.is_up() {
                return Ok(());
            }
            self.start(db).await.map(|_| ())
        } else {
            self.stop(db).await;
            Ok(())
        }
    }

    fn is_up(&self) -> bool {
        self.tunnel.lock().unwrap().is_some()
    }

    /// Register (first enable only), stand the tunnel up and point the whole app at it. Returns
    /// the `(SOCKS5 port, HTTP proxy port)` the tunnel ended up on. Fails without touching the
    /// proxy, so a broken tunnel never takes playback down with it.
    pub async fn start(&self, db: &Db) -> Result<(u16, u16), String> {
        self.set_status(WarpStatus::Registering);
        let device =
            match db.get_setting("warp_device_json").as_deref().and_then(WarpDevice::from_json) {
                Some(d) => d,
                None => {
                    let d = register().await.map_err(|e| {
                        let msg = format!("WARP registration failed: {e}");
                        self.set_status(WarpStatus::Failed { msg: msg.clone() });
                        msg
                    })?;
                    db.set_setting("warp_device_json", &d.to_json());
                    d
                }
            };

        self.set_status(WarpStatus::Connecting);
        let tunnel = start_tunnel(&device).await.map_err(|e| {
            let msg = format!("WARP tunnel failed: {e}");
            self.set_status(WarpStatus::Failed { msg: msg.clone() });
            msg
        })?;

        let (socks_port, http_port) = (tunnel.socks_port, tunnel.http_port);
        apply_tunnel_proxy(self.app.clone(), socks_port, http_port);
        spawn_watchdog(self.app.clone(), device, tunnel.shutdown_rx());
        *self.tunnel.lock().unwrap() = Some(tunnel);
        self.set_status(WarpStatus::Connected);
        Ok((socks_port, http_port))
    }

    /// Tear the tunnel down and put the manual `proxy` setting (or "direct") back.
    pub async fn stop(&self, db: &Db) {
        if let Some(tunnel) = self.tunnel.lock().unwrap().take() {
            tunnel.kill();
            apply_manual_proxy(self.app.clone(), db.get_setting("proxy").as_deref());
        }
        self.set_status(WarpStatus::Off);
    }
}

/// The proxy URL the API clients and the webviews are pointed at while the tunnel is up.
/// `socks5://`, not `socks5h://`: reqwest resolves DNS locally and hands the proxy an IP, and the
/// webview side only accepts this scheme shape (`http::webview_url`).
pub(crate) fn socks_url(port: u16) -> String {
    format!("socks5://127.0.0.1:{port}")
}

/// The proxy URL mpv is pointed at: libavformat only speaks `http://` proxies, so the streams go
/// through the HTTP CONNECT front, not SOCKS.
pub(crate) fn http_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// One place knows how to re-point the whole app at the tunnel: the shared reqwest client pool
/// (`http.rs`), mpv's own http proxy (HTTP CONNECT front — see [`http_url`]), and the InnerTube
/// transport behind the orchestrator.
fn apply_tunnel_proxy(app: AppHandle, socks: u16, http: u16) {
    let socks_url = socks_url(socks);
    crate::http::set_proxy(Some(&socks_url));
    if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
        if let Err(e) = state.player.set_http_proxy(Some(&http_url(http))) {
            tracing::warn!("mpv did not take the new proxy: {e}");
        }
        if let Err(e) = state.orchestrator.set_proxy(Some(&socks_url)) {
            tracing::warn!("the InnerTube transport did not take the new proxy: {e}");
        }
    }
}

/// The tunnel's mirror image: everything back onto the user's manual `proxy` setting (or direct).
/// Same plumbing, so the restore is as complete as the switch.
fn apply_manual_proxy(app: AppHandle, proxy: Option<&str>) {
    crate::http::set_proxy(proxy);
    if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
        if let Err(e) = state.player.set_http_proxy(proxy) {
            tracing::warn!("mpv did not take the restored proxy: {e}");
        }
        if let Err(e) = state.orchestrator.set_proxy(proxy) {
            tracing::warn!("the InnerTube transport did not take the restored proxy: {e}");
        }
    }
}

/// Watchdog interval and how the health probe talks to YouTube through the front. It probes the
/// same way the app's own clients do — through the SOCKS5 front — so a dead front is a dead
/// tunnel for the probe too.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(20);
const WATCHDOG_PROBE_URL: &str = "https://1.1.1.1/cdn-cgi/trace";
const WATCHDOG_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Consecutive failures before the interface is rebuilt. One miss happens on flaky mobile
/// networks; two in a row means the session is gone for good.
const WATCHDOG_STRIKES: u32 = 2;

/// The WireGuard session is not bulletproof: handshakes on flaky networks (CGNAT, mobile data)
/// sometimes die mid-session and do not always come back on their own. This loop probes the front
/// every [`WATCHDOG_INTERVAL`] and, after [`WATCHDOG_STRIKES`] consecutive misses, tears the
/// interface down and rebuilds it from the same device — the ports may change, so every consumer
/// is re-pointed (`apply_tunnel_proxy`). The user never sees more than a brief `connecting` blip.
fn spawn_watchdog(app: AppHandle, device: WarpDevice, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut shutdown = shutdown;
        let mut fails = 0u32;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(WATCHDOG_INTERVAL) => {}
            }

            let probe = tokio::time::timeout(WATCHDOG_PROBE_TIMEOUT, async {
                crate::http::client()
                    .get(WATCHDOG_PROBE_URL)
                    .timeout(WATCHDOG_PROBE_TIMEOUT)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false);

            if probe {
                fails = 0;
                continue;
            }
            fails += 1;
            tracing::warn!(fails, "WARP tunnel health probe failed");

            if fails < WATCHDOG_STRIKES {
                continue;
            }
            fails = 0;

            let Some(state) = app.try_state::<Arc<crate::state::AppState>>() else { continue };
            let manager = state.warp.clone();
            if !manager.is_up() {
                return;
            }
            manager.set_status(WarpStatus::Connecting);
            tracing::warn!("re-establishing the WARP interface");

            // Drop the old front ends first so nothing accepts while the new one stands up.
            let old = { manager.tunnel.lock().unwrap().take() };
            if let Some(old) = old {
                old.kill();
            }
            match start_tunnel(&device).await {
                Ok(tunnel) => {
                    apply_tunnel_proxy(app.clone(), tunnel.socks_port, tunnel.http_port);
                    spawn_watchdog(app.clone(), device.clone(), tunnel.shutdown_rx());
                    *manager.tunnel.lock().unwrap() = Some(tunnel);
                    manager.set_status(WarpStatus::Connected);
                }
                Err(e) => {
                    tracing::error!("WARP interface rebuild failed: {e}");
                    manager.set_status(WarpStatus::Failed { msg: e });
                    return;
                }
            }
        }
    });
}

/// Ask Cloudflare for a fresh anonymous WARP device (the `wgcf` flow): generate a WireGuard
/// keypair, POST it with a plausible Android-client shape, keep back what the tunnel needs.
/// Anonymous: no account, no email — the device is its own credential.
async fn register() -> Result<WarpDevice, String> {
    let kp = generate_keypair();
    let tos = time_rfc3339();
    let serial = uuid::Uuid::new_v4().to_string();

    let resp = crate::http::client()
        .post(REG_URL)
        .header("User-Agent", REG_UA)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "install_id": "",
            "fcm_token": "",
            "key": kp.public_key_b64,
            "tos": tos,
            "model": "PC",
            "serial_number": serial,
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let body: serde_json::Value =
        resp.json().await.map_err(|e| format!("unreadable registration reply ({status}): {e}"))?;
    if !status.is_success() {
        let msg =
            body.get("errors").map(|e| e.to_string()).unwrap_or_else(|| "unknown error".into());
        return Err(format!("the API answered {status}: {msg}"));
    }

    // Pointer paths, so an extra field or reordering on their side does not break the read.
    let cfg = body.get("config").ok_or("registration reply has no config")?;
    let v4 = cfg
        .pointer("/interface/addresses/v4")
        .and_then(|v| v.as_str())
        .ok_or("registration reply has no v4 address")?
        .to_owned();
    let peer = cfg.pointer("/peers/0").ok_or("registration reply has no peer")?;
    let peer_public = peer
        .get("public_key")
        .and_then(|v| v.as_str())
        .ok_or("registration reply has no peer public key")?
        .to_owned();
    // `endpoint.host` arrives as `engage.cloudflareclient.com:2408` with `endpoint.port` null and
    // alternates in `endpoint.ports` — so take the port out of the host, falling back to the
    // first listed one when the host ships bare.
    let host = peer
        .pointer("/endpoint/host")
        .and_then(|v| v.as_str())
        .ok_or("registration reply has no endpoint host")?;
    let endpoint = if host.contains(':') {
        host.to_owned()
    } else {
        let port = peer.pointer("/endpoint/ports/0").and_then(|v| v.as_u64()).unwrap_or(2408);
        format!("{host}:{port}")
    };

    Ok(WarpDevice {
        private_key_b64: kp.private_key_b64,
        v4_address: v4,
        peer_public_key_b64: peer_public,
        endpoint,
    })
}

/// `now` in the RFC3339 shape the registration body's `tos` field wants.
fn time_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

/// Days-since-epoch → calendar date (Howard Hinnant's civil_from_days), so no date crate joins
/// the tree for one timestamp.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

/// Bind the first free port in a small range: the bases are picked so collisions need a second
/// tunnel from something else, but the machine may hold another one of ours from a crashed run.
async fn bind_first_free(base: u16) -> Option<(TcpListener, u16)> {
    for i in 0..PORT_ATTEMPTS {
        let bind: SocketAddr = format!("127.0.0.1:{}", base + i).parse().expect("static bind");
        if let Ok(listener) = TcpListener::bind(bind).await {
            return Some((listener, bind.port()));
        }
    }
    None
}

/// Parse the device into a `.conf`, bring the engine up, and stand both front ends over it. The
/// engine owns the WireGuard session (handshakes, rekeys, the 10 s keepalive); the front ends
/// only translate between the app's sockets and it.
async fn start_tunnel(device: &WarpDevice) -> Result<Tunnel, String> {
    let config = Config::parse(&device.wg_conf())
        .map_err(|e| format!("the generated WARP config is invalid: {e}"))?;

    // Same startup rule as wgproxyrs itself: resolve the WireGuard endpoint once, here.
    let peer = config.peers.first().ok_or("generated config has no peer")?;
    let endpoint: SocketAddr = peer
        .endpoint
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve the WARP endpoint {}: {e}", peer.endpoint))?
        .next()
        .ok_or_else(|| format!("endpoint {} resolved to nothing", peer.endpoint))?;
    let udp_bind = SocketAddr::new(
        if endpoint.is_ipv6() {
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        } else {
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        },
        config.interface.listen_port.unwrap_or(0),
    );
    let transport = UdpTransport::new(udp_bind, endpoint)
        .map_err(|e| format!("cannot bind the tunnel UDP socket: {e}"))?;
    let peers = vec![PeerTunnel {
        tunnel: WgTunnel::new(
            config.interface.private_key,
            peer.public_key,
            peer.preshared_key,
            peer.persistent_keepalive,
            Box::new(transport),
        ),
        allowed_ips: peer.allowed_ips.clone(),
        endpoint: peer.endpoint.clone(),
    }];

    let (engine, _engine_task) = spawn_engine(
        peers,
        EngineConfig {
            addresses: config.interface.addresses.clone(),
            dns_servers: config.interface.dns.clone(),
            mtu: usize::from(wgproxyrs::config::resolve_mtu(None, config.interface.mtu)),
        },
    );

    let (socks_listener, socks_port) = bind_first_free(SOCKS_PORT_BASE).await.ok_or_else(|| {
        format!(
            "no free port for the SOCKS5 listener in {SOCKS_PORT_BASE}..{}",
            SOCKS_PORT_BASE + PORT_ATTEMPTS
        )
    })?;
    let (http_listener, http_port) = bind_first_free(HTTP_PORT_BASE).await.ok_or_else(|| {
        format!(
            "no free port for the HTTP listener in {HTTP_PORT_BASE}..{}",
            HTTP_PORT_BASE + PORT_ATTEMPTS
        )
    })?;
    tracing::info!(
        "WARP tunnel up: socks5 on 127.0.0.1:{socks_port}, http proxy on 127.0.0.1:{http_port}"
    );

    let ctx = ProxyCtx::new(engine.clone());
    // The front loops return `io::Result<()>` that only matters when they die; the abort handles
    // shutdown, and the engine being gone makes them exit on their own.
    let socks_ctx = ctx.clone();
    let socks_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        let _ = run_socks5_listener(socks_listener, socks_ctx).await;
    });
    let http_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        let _ = run_http_listener(http_listener, ctx).await;
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    Ok(Tunnel {
        engine,
        front_tasks: vec![socks_task, http_task],
        shutdown: shutdown_tx,
        socks_port,
        http_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_blob_round_trips() {
        let d = WarpDevice {
            // 44 characters: standard base64 of 32 bytes (the API's own key shape).
            private_key_b64: format!("{}=", "a".repeat(43)),
            v4_address: "172.16.0.2".into(),
            peer_public_key_b64: format!("{}=", "b".repeat(43)),
            endpoint: "engage.cloudflareclient.com:2408".into(),
        };
        let json = d.to_json();
        let back = WarpDevice::from_json(&json).expect("round trip");
        assert_eq!(back.endpoint, d.endpoint);
        assert_eq!(back.v4_address, d.v4_address);
    }

    #[test]
    fn generated_conf_is_parseable_and_carries_the_essentials() {
        let d = WarpDevice {
            // base64 of 32 zero bytes: the residual bits before the padding are zero, which is
            // what strict WireGuard key decoders (boringtun's, here) demand.
            private_key_b64: format!("{}=", "A".repeat(43)),
            v4_address: "172.16.0.2".into(),
            peer_public_key_b64: format!("{}=", "A".repeat(43)),
            endpoint: "engage.cloudflareclient.com:2408".into(),
        };
        let config = Config::parse(&d.wg_conf()).expect("the generated conf must parse");
        assert_eq!(config.interface.addresses.len(), 1);
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].endpoint, d.endpoint);
        assert_eq!(config.peers[0].persistent_keepalive, Some(KEEPALIVE_SECS));
        // The engine resolves domain targets through the tunnel, so DNS must be pinned.
        assert!(config.interface.dns.contains(&"1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn socks_url_is_webview_shaped() {
        assert!(crate::http::webview_url(&socks_url(51879)).is_some());
    }

    #[test]
    fn civil_date_is_correct() {
        // 2026-01-01T00:00:00Z is 1767225600; noon the same day shares the date part.
        assert_eq!(civil_from_unix(1_767_225_600), (2026, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_767_225_600 + 12 * 3600), (2026, 1, 1, 12, 0, 0));
    }

    #[test]
    fn rfc3339_shape() {
        let t = time_rfc3339();
        assert_eq!(t.len(), 24);
        assert!(t.ends_with('Z'));
        assert_eq!(&t[4..5], "-");
    }
}
