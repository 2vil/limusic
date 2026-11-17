//! The embedded WireGuard tunnel (Settings ▸ Network, the "experimental" toggle).
//!
//! YT Music is not served in every country; users there need their traffic to come from an IP in
//! a served one. The app already routes everything through the `proxy` setting (#241), so what is
//! missing is a provider: the user imports any WireGuard `.conf` (Mullvad, Proton, a `wgcf`-made
//! WARP profile — their pick), the app embeds a userspace WireGuard engine over it and points the
//! proxy plumbing at that. Nothing is system-wide: no TUN device, no routes, no root — the two
//! front ends listen on 127.0.0.1 and only this app's traffic is pointed at them. Desktop and
//! Android alike.
//!
//! Three pieces, in dependency order:
//! 1. **The imported config** ([`imported_conf`]): the `.conf` text lives in the database (it
//!    holds the private key, so it must never cross into the webview and must not join
//!    `UI_SETTINGS`). A `DNS` line and a `PersistentKeepalive` are injected when missing — the
//!    engine resolves domain targets through the tunnel, and a missing keepalive lets NAT boxes
//!    on flaky networks drop the session silently.
//! 2. **The engine** ([`start_tunnel`]): `wgproxyrs` — a userspace WireGuard engine (boringtun +
//!    smoltcp) that keeps the session alive, re-handshakes on its own and resolves domain targets
//!    through the tunnel's own DNS. Chosen after the first engine this module carried
//!    (`tokio-wireguard`) dropped its session mid-flight on CGNAT-flaky networks no matter how it
//!    was wired; the same probe battery that killed it runs clean on `wgproxyrs` for as long as
//!    it was run.
//! 3. **The fronts**: `wgproxyrs` serves SOCKS5 (`run_socks5_listener`) and HTTP CONNECT
//!    (`run_http_listener`) over one engine — mpv needs the latter, because libavformat only
//!    speaks `http://` proxies (see `player::set_http_proxy`).
//!
//! A watchdog ([`spawn_watchdog`]) probes the front every so often and rebuilds the interface on
//! two consecutive misses, re-pointing every consumer (the ports can change).
//!
//! State machine: `Off → Connecting → Connected`, any step failing → `Failed(msg)` with the
//! proxy untouched. Toggling off tears the engine down and restores the user's manual `proxy`
//! setting.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;
use tokio::sync::watch;
use wgproxyrs::config::Config;
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
/// Aggressive on purpose: the first engine lost its session on networks whose NAT forgets UDP
/// flows faster than the `wg`-default 25 s, and 10 s costs nothing.
const KEEPALIVE_SECS: u16 = 10;
/// The DNS the engine resolves domain targets with, injected into configs that ship none.
const FALLBACK_DNS: &str = "1.1.1.1";

/// What the UI is told, through the `warp-status` event and the `warp_status` command. Serialized
/// with the tag on the outside so JS sees `{ status: "connected" }` / `{ status: "failed", msg }`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WarpStatus {
    Off,
    Connecting,
    Connected,
    Failed { msg: String },
}

/// The imported `.conf`, as the engine needs it: a `DNS` line and a `PersistentKeepalive` are
/// injected when the file ships none. The engine resolves domain targets through the tunnel (its
/// own smoltcp DNS socket, fed by `DNS`), so a config without one would make every domain connect
/// fail; and without a keepalive, NAT boxes on flaky networks drop the session silently.
fn normalize_conf(conf: &str) -> String {
    let mut out = conf.trim().to_owned();
    if !out.contains("DNS") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("DNS = {FALLBACK_DNS}\n"));
    }
    if !out.contains("PersistentKeepalive") {
        out.push_str(&format!("PersistentKeepalive = {KEEPALIVE_SECS}\n"));
    }
    out
}

/// The stored `.conf`, or `None` until one is imported.
fn imported_conf(db: &Db) -> Option<String> {
    db.get_setting("warp_conf").filter(|c| !c.trim().is_empty()).map(|c| normalize_conf(&c))
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

    /// Whether a tunnel is currently standing. The import command uses it to offer a live swap.
    pub(crate) fn is_up(&self) -> bool {
        self.tunnel.lock().unwrap().is_some()
    }

    /// Stand the tunnel up over the imported `.conf` and point the whole app at it. Returns the
    /// `(SOCKS5 port, HTTP proxy port)` the tunnel ended up on. Fails without touching the proxy,
    /// so a broken tunnel never takes playback down with it.
    pub async fn start(&self, db: &Db) -> Result<(u16, u16), String> {
        self.set_status(WarpStatus::Connecting);
        let conf = match imported_conf(db) {
            Some(c) => c,
            None => {
                let msg = "no WireGuard config imported yet".to_owned();
                self.set_status(WarpStatus::Failed { msg: msg.clone() });
                return Err(msg);
            }
        };

        let tunnel = start_tunnel(&conf).await.map_err(|e| {
            let msg = format!("WireGuard tunnel failed: {e}");
            self.set_status(WarpStatus::Failed { msg: msg.clone() });
            msg
        })?;

        let (socks_port, http_port) = (tunnel.socks_port, tunnel.http_port);
        apply_tunnel_proxy(self.app.clone(), socks_port, http_port);
        spawn_watchdog(self.app.clone(), conf, tunnel.shutdown_rx());
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

    /// Swap the tunnel over to whatever `.conf` is now in the database, without leaving the user
    /// on the old one. A no-op while the tunnel is off (the next start picks it up).
    pub(crate) async fn restart(&self, db: &Db) -> Result<(), String> {
        if self.is_up() {
            self.stop(db).await;
            self.start(db).await.map(|_| ())
        } else {
            Ok(())
        }
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
/// interface down and rebuilds it from the same imported config — the ports may change, so every
/// consumer is re-pointed (`apply_tunnel_proxy`). The user never sees more than a brief
/// `connecting` blip.
fn spawn_watchdog(app: AppHandle, conf: String, shutdown: watch::Receiver<bool>) {
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
            tracing::warn!("re-establishing the WireGuard interface");

            // Drop the old front ends first so nothing accepts while the new one stands up.
            let old = { manager.tunnel.lock().unwrap().take() };
            if let Some(old) = old {
                old.kill();
            }
            match start_tunnel(&conf).await {
                Ok(tunnel) => {
                    apply_tunnel_proxy(app.clone(), tunnel.socks_port, tunnel.http_port);
                    spawn_watchdog(app.clone(), conf.clone(), tunnel.shutdown_rx());
                    *manager.tunnel.lock().unwrap() = Some(tunnel);
                    manager.set_status(WarpStatus::Connected);
                }
                Err(e) => {
                    tracing::error!("WireGuard interface rebuild failed: {e}");
                    manager.set_status(WarpStatus::Failed { msg: e });
                    return;
                }
            }
        }
    });
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

/// Parse the imported `.conf`, bring the engine up, and stand both front ends over it. The
/// engine owns the WireGuard session (handshakes, rekeys, the keepalive); the front ends only
/// translate between the app's sockets and it.
async fn start_tunnel(conf: &str) -> Result<Tunnel, String> {
    let config =
        Config::parse(conf).map_err(|e| format!("the WireGuard config is invalid: {e}"))?;

    // Same startup rule as wgproxyrs itself: resolve the WireGuard endpoint once, here.
    let peer = config.peers.first().ok_or("the config has no [Peer] section")?;
    let endpoint: SocketAddr = peer
        .endpoint
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve the tunnel endpoint {}: {e}", peer.endpoint))?
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

    let (shutdown_tx, _) = watch::channel(false);
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

    /// A minimal but fully valid client config, the shape users import.
    fn sample_conf() -> String {
        // base64 of 32 zero bytes: the residual bits before the padding are zero, which is what
        // strict WireGuard key decoders (boringtun's, here) demand.
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 172.16.0.2/32\n\n[Peer]\n\
             PublicKey = {}\nAllowedIPs = 0.0.0.0/0\n\
             Endpoint = engage.cloudflareclient.com:2408\n",
            format!("{}=", "A".repeat(43)),
            format!("{}=", "A".repeat(43)),
        )
    }

    #[test]
    fn normalize_injects_dns_and_keepalive() {
        let conf = normalize_conf(&sample_conf());
        let config = Config::parse(&conf).expect("the normalized conf must parse");
        // The engine resolves domain targets through the tunnel, so DNS must be pinned.
        assert!(config.interface.dns.contains(&"1.1.1.1".parse().unwrap()));
        assert_eq!(config.peers[0].persistent_keepalive, Some(KEEPALIVE_SECS));
        assert_eq!(config.interface.addresses.len(), 1);
        assert_eq!(config.peers.len(), 1);
    }

    #[test]
    fn normalize_leaves_complete_configs_alone() {
        let conf = format!("{}\nDNS = 9.9.9.9\nPersistentKeepalive = 15\n", sample_conf());
        let config = Config::parse(&normalize_conf(&conf)).expect("parses");
        assert!(config.interface.dns.contains(&"9.9.9.9".parse().unwrap()));
        assert_eq!(config.peers[0].persistent_keepalive, Some(15));
    }

    #[test]
    fn socks_url_is_webview_shaped() {
        assert!(crate::http::webview_url(&socks_url(51879)).is_some());
    }
}
