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
//! 2. **The tunnel** ([`start_tunnel`]): an in-process WireGuard endpoint (`tokio-wireguard`,
//!    which is boringtun + a smoltcp TCP/IP stack) carrying the registered peer.
//! 3. **The SOCKS5 front** ([`serve_socks`]): a `fast-socks5` listener that pipes every accepted
//!    CONNECT into a [`tokio_wireguard::TcpStream`] over that interface. Shaped after the
//!    271-line `wg-socks` crate; inlined here so status, shutdown and port fallback are ours.
//!
//! DNS is resolved locally by design (`socks5://`, not `socks5h://`): the geo-check YouTube does
//! is IP-based, and the tunnel's stack is registered v4-only, so domain targets are resolved here
//! and pinned to their first IPv4 answer.
//!
//! State machine: `Off → Registering → Connecting → Connected`, any step failing → `Failed(msg)`
//! with the proxy untouched. Toggling off tears the tunnel down and restores the user's manual
//! `proxy` setting.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use fast_socks5::server::Socks5ServerProtocol;
use fast_socks5::util::target_addr::TargetAddr;
use fast_socks5::{ReplyError, Socks5Command};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_wireguard::config::{Config, Interface as WgInterface, Peer};
use tokio_wireguard::interface::ToInterface;

use crate::db::Db;

/// WARP peers forward everything, so the tunnel's routing table is "the world". v4 only: the
/// interface itself is registered with an IPv4 address, and a v6 route through it would be
/// pretend anyway.
const ALL_V4: &str = "0.0.0.0/0";
/// NAT keepalive: WARP endpoints expect the client to poke through, and the phone side benefits
/// most — a silent tunnel on mobile data dies at the carrier's NAT in minutes.
const KEEPALIVE_SECS: u16 = 25;
/// Base port for the local SOCKS5 listener. Picked once so the webview proxy URL is stable across
/// launches; on a collision the next few ports are tried before giving up.
const SOCKS_PORT_BASE: u16 = 51879;
/// How many ports past the base [`start_tunnel`] will try before failing.
const SOCKS_PORT_ATTEMPTS: u16 = 5;

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

    fn config(&self) -> Result<Config, String> {
        use base64::Engine;
        // WireGuard key material travels in standard base64 (the `wg` config format), both in
        // what we send to the registration API and what it sends back.
        let engine = base64::engine::general_purpose::STANDARD;
        let decode32 = |s: &str| -> Result<[u8; 32], String> {
            let raw = engine.decode(s).map_err(|e| e.to_string())?;
            if raw.len() != 32 {
                return Err(format!("key is {} bytes, expected 32", raw.len()));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&raw);
            Ok(out)
        };
        let private = boringtun::x25519::StaticSecret::from(decode32(&self.private_key_b64)?);
        let public = boringtun::x25519::PublicKey::from(decode32(&self.peer_public_key_b64)?);
        // The API hands back a bare `172.16.0.2`; the interface wants a network (`/32`), which is
        // what `wgcf` writes into the configs it emits.
        let v4 = if self.v4_address.contains('/') {
            self.v4_address.clone()
        } else {
            format!("{}/32", self.v4_address)
        };
        let address: tokio_wireguard::config::Address =
            v4.parse().map_err(|e| format!("bad WARP address {v4}: {e}"))?;
        let endpoint = resolve_endpoint(&self.endpoint)?;
        Ok(Config {
            interface: WgInterface { private_key: private, address, listen_port: None, mtu: None },
            peers: vec![Peer {
                public_key: public,
                endpoint: Some(endpoint),
                allowed_ips: vec![ALL_V4.parse().map_err(|e| format!("bad AllowedIPs: {e}"))?],
                persistent_keepalive: Some(KEEPALIVE_SECS),
            }],
        })
    }
}

/// `engage.cloudflareclient.com:2408` is a DNS name; the interface wants a `SocketAddr`. Resolved
/// at tunnel start (not registration time): the answer changes, and resolving late means a saved
/// device works however long it sat unused.
fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, String> {
    let (host, port) =
        endpoint.rsplit_once(':').ok_or_else(|| format!("endpoint {endpoint} has no port"))?;
    let port: u16 = port.parse().map_err(|_| format!("bad endpoint port in {endpoint}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let found =
        std::net::ToSocketAddrs::to_socket_addrs(&(host, port)).ok().and_then(|mut it| it.next());
    found.ok_or_else(|| format!("could not resolve the WARP endpoint {endpoint}"))
}

pub struct WarpManager {
    app: AppHandle,
    status: RwLock<WarpStatus>,
    tunnel: Mutex<Option<Tunnel>>,
}

struct Tunnel {
    shutdown: watch::Sender<bool>,
    port: u16,
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
    /// the SOCKS5 port the tunnel ended up on. Fails without touching the proxy, so a broken
    /// tunnel never takes playback down with it.
    pub async fn start(&self, db: &Db) -> Result<u16, String> {
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

        let port = tunnel.port;
        apply_proxy(self.app.clone(), Some(&socks_url(port)));
        *self.tunnel.lock().unwrap() = Some(tunnel);
        self.set_status(WarpStatus::Connected);
        Ok(port)
    }

    /// Tear the tunnel down and put the manual `proxy` setting (or "direct") back.
    pub async fn stop(&self, db: &Db) {
        if let Some(tunnel) = self.tunnel.lock().unwrap().take() {
            let _ = tunnel.shutdown.send(true);
            apply_proxy(self.app.clone(), db.get_setting("proxy").as_deref());
        }
        self.set_status(WarpStatus::Off);
    }
}

/// The proxy URL the app's plumbing is pointed at while the tunnel is up. `socks5://`, not
/// `socks5h://`: reqwest resolves DNS locally and hands the proxy an IP (see the module doc), and
/// the webview side only accepts this scheme shape (`http::webview_url`).
fn socks_url(port: u16) -> String {
    format!("socks5://127.0.0.1:{port}")
}

/// One place knows how to re-point the whole app: the shared reqwest client pool (`http.rs`),
/// mpv's own http proxy, and the InnerTube transport behind the orchestrator. The state may not
/// be around during early startup (the tunnel starts before `AppState` is managed) — then
/// whoever builds the clients next reads the current setting anyway.
fn apply_proxy(app: AppHandle, proxy: Option<&str>) {
    crate::http::set_proxy(proxy);
    if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
        if let Err(e) = state.player.set_http_proxy(proxy) {
            tracing::warn!("mpv did not take the new proxy: {e}");
        }
        if let Err(e) = state.orchestrator.set_proxy(proxy) {
            tracing::warn!("the InnerTube transport did not take the new proxy: {e}");
        }
    }
}

/// Ask Cloudflare for a fresh anonymous WARP device (the `wgcf` flow): generate a WireGuard
/// keypair, POST it with a plausible Android-client shape, keep back what the tunnel needs.
/// Anonymous: no account, no email — the device is its own credential.
async fn register() -> Result<WarpDevice, String> {
    use base64::Engine;
    let (private, public) = tokio_wireguard::x25519::keypair();
    // Standard base64 with padding: it is the `wg` config key format, and the intake rejects
    // anything else ("Invalid public key", code 1051).
    let engine = base64::engine::general_purpose::STANDARD;
    let key = engine.encode(public.as_bytes());
    let tos = time_rfc3339();
    let serial = uuid::Uuid::new_v4().to_string();

    let resp = crate::http::client()
        .post(REG_URL)
        .header("User-Agent", REG_UA)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "install_id": "",
            "fcm_token": "",
            "key": key,
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
        private_key_b64: engine.encode(private.to_bytes()),
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

/// Build the WireGuard interface, open the SOCKS5 listener (with the small port fallback) and
/// hand the accept loop a shutdown channel. The accept loop runs for the tunnel's lifetime; the
/// caller only keeps the handle.
async fn start_tunnel(device: &WarpDevice) -> Result<Tunnel, String> {
    let config = device.config()?;
    let interface = config
        .to_interface()
        .await
        .map_err(|e| format!("could not bring the WireGuard interface up: {e}"))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    for attempt in 0..SOCKS_PORT_ATTEMPTS {
        let bind: SocketAddr = format!("127.0.0.1:{}", SOCKS_PORT_BASE + attempt)
            .parse()
            .expect("static bind address");
        let Ok(listener) = TcpListener::bind(bind).await else {
            continue;
        };
        tracing::info!("WARP tunnel up, SOCKS5 listening on {bind}");
        let iface = interface.clone();
        tokio::spawn(serve_socks(listener, iface, shutdown_rx));
        return Ok(Tunnel { shutdown: shutdown_tx, port: bind.port() });
    }
    Err(format!(
        "no free port for the SOCKS5 listener in {SOCKS_PORT_BASE}..{}",
        SOCKS_PORT_BASE + SOCKS_PORT_ATTEMPTS
    ))
}

/// Accept loop: one task per client, `CONNECT` only, piped into the tunnel. Same shape as the
/// reference `wg-socks` server, with the target resolved to IPv4 (the tunnel is v4-only, and
/// handing a v6 address to a v4 stack fails per-connection where resolving first fails fast).
async fn serve_socks(
    listener: TcpListener,
    interface: tokio_wireguard::Interface,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, peer)) => {
                        let iface = interface.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(socket, iface).await {
                                tracing::debug!("WARP client {peer}: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("WARP SOCKS5 accept error: {e}");
                    }
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("WARP tunnel shutting down");
                return;
            }
        }
    }
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    interface: tokio_wireguard::Interface,
) -> Result<(), String> {
    let (proto, cmd, target) = Socks5ServerProtocol::accept_no_auth(socket)
        .await
        .map_err(|e| e.to_string())?
        .read_command()
        .await
        .map_err(|e| e.to_string())?;

    if cmd != Socks5Command::TCPConnect {
        let _ = proto.reply_error(&ReplyError::CommandNotSupported).await;
        return Err(format!("unsupported SOCKS command: {cmd:?}"));
    }

    let addr = match target {
        TargetAddr::Ip(ip) => ip,
        TargetAddr::Domain(name, port) => {
            let resolved = tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(|e| format!("dns for {name}: {e}"))?
                .find(|a| a.is_ipv4())
                .ok_or_else(|| format!("no IPv4 answer for {name}"));
            match resolved {
                Ok(a) => a,
                Err(e) => {
                    let _ = proto.reply_error(&ReplyError::HostUnreachable).await;
                    return Err(e);
                }
            }
        }
    };

    let stream = tokio_wireguard::TcpStream::connect(addr, interface).await;
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = proto.reply_error(&ReplyError::HostUnreachable).await;
            return Err(format!("tunnel connect to {addr}: {e}"));
        }
    };

    // `reply_success` hands the underlying socket back with the reply sent, ready for piping.
    let client = proto
        .reply_success(SocketAddr::from(([0, 0, 0, 0], 0)))
        .await
        .map_err(|e| e.to_string())?;

    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut tun_rx, mut tun_tx) = tokio::io::split(stream);
    tokio::select! {
        r = tokio::io::copy(&mut client_rx, &mut tun_tx) => { r.map_err(|e| e.to_string())?; }
        r = tokio::io::copy(&mut tun_rx, &mut client_tx) => { r.map_err(|e| e.to_string())?; }
    }
    Ok(())
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
