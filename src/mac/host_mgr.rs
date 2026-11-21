//! Multi-host connection manager (`HostManager`) for the macOS app.
//!
//! Maintains persistent multiplexed `MuxClient` connections to `local`
//! ("This Mac") and every configured remote host in `~/.config/Tachyon/hosts.json`
//! (such as `kagutsuchi`). Each host runs on its own background worker thread
//! that handles:
//! - initial connect (auto-starting local `tachyon server`, or spawning
//!   `ssh -T ... tachyon bridge` for remote hosts),
//! - live `events.subscribe` streaming to keep the sidebar and split layouts
//!   synchronized with CLI / coding-agent actions in real time,
//! - periodic `PING` heartbeats for RTT display and dead-connection detection,
//! - exponential-backoff auto-reconnect after sleep or network changes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use dispatch2::DispatchQueue;
use parking_lot::Mutex;
use serde_json::Value;

use crate::mux::client::MuxClient;
use crate::mux::model::{HostEntry, HostInfo, ServerSnapshot};
use crate::mux::{default_socket_path, load_hosts, resolve_target, save_hosts, Target};

#[derive(Clone, Debug, PartialEq)]
pub enum HostStatus {
    Connecting,
    Connected { info: HostInfo, rtt_ms: Option<f64> },
    Disconnected { error: String },
}

/// Read-only snapshot of one host's state for rendering in the sidebar.
#[derive(Clone, Debug)]
pub struct HostView {
    pub alias: String,
    pub display_name: String,
    pub uri: String,
    pub is_local: bool,
    pub color: Option<String>,
    pub status: HostStatus,
    pub snapshot: Option<ServerSnapshot>,
}

struct ManagedHost {
    alias: String,
    uri: String,
    is_local: bool,
    color: Option<String>,
    status: HostStatus,
    snapshot: Option<ServerSnapshot>,
    client: Option<Arc<MuxClient>>,
    stop: Arc<AtomicBool>,
    wake: Sender<()>,
}

type ChangeCallback = Arc<dyn Fn() + Send + Sync>;
type EventCallback = Arc<dyn Fn(String, Value) + Send + Sync>;

pub struct HostManager {
    order: Mutex<Vec<String>>,
    hosts: Mutex<HashMap<String, ManagedHost>>,
    on_change: ChangeCallback,
    on_event: EventCallback,
}

impl HostManager {
    /// Start managing `local` plus all hosts saved in `~/.config/Tachyon/hosts.json`.
    pub fn start(
        on_change: impl Fn() + Send + Sync + 'static,
        on_event: impl Fn(String, Value) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let mgr = Arc::new(Self {
            order: Mutex::new(Vec::new()),
            hosts: Mutex::new(HashMap::new()),
            on_change: Arc::new(on_change),
            on_event: Arc::new(on_event),
        });

        // Always register `local` first ("This Mac").
        let sock = default_socket_path();
        mgr.spawn_host(
            "local".to_string(),
            format!("unix://{}", sock.display()),
            Target::Local { socket: sock },
            true,
            None,
        );

        let entries = load_hosts();
        for entry in entries {
            if entry.alias == "local" {
                continue;
            }
            if let Ok(target) = resolve_target(Some(&entry.alias), std::slice::from_ref(&entry)) {
                mgr.spawn_host(entry.alias, entry.uri, target, false, entry.color);
            }
        }
        mgr
    }

    fn spawn_host(
        self: &Arc<Self>,
        alias: String,
        uri: String,
        target: Target,
        is_local: bool,
        color: Option<String>,
    ) {
        let (wake_tx, wake_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut order = self.order.lock();
            if !order.contains(&alias) {
                order.push(alias.clone());
            }
            let mut hosts = self.hosts.lock();
            if let Some(prev) = hosts.remove(&alias) {
                prev.stop.store(true, Ordering::Release);
                let _ = prev.wake.send(());
            }
            hosts.insert(
                alias.clone(),
                ManagedHost {
                    alias: alias.clone(),
                    uri,
                    is_local,
                    color,
                    status: HostStatus::Connecting,
                    snapshot: None,
                    client: None,
                    stop: Arc::clone(&stop),
                    wake: wake_tx,
                },
            );
        }

        let mgr = Arc::clone(self);
        let worker_alias = alias.clone();
        let _ = std::thread::Builder::new()
            .name(format!("tachyon-host-{alias}"))
            .spawn(move || host_worker(mgr, worker_alias, target, is_local, stop, wake_rx));
    }

    fn notify_change(&self) {
        let cb = Arc::clone(&self.on_change);
        DispatchQueue::main().exec_async(move || cb());
    }

    fn notify_event(&self, host: String, ev: Value) {
        let cb = Arc::clone(&self.on_event);
        DispatchQueue::main().exec_async(move || cb(host, ev));
    }

    /// Snapshot of all hosts in display order (`local` first, then remote hosts).
    pub fn views(&self) -> Vec<HostView> {
        let order = self.order.lock().clone();
        let hosts = self.hosts.lock();
        order
            .into_iter()
            .filter_map(|alias| {
                let h = hosts.get(&alias)?;
                Some(HostView {
                    alias: h.alias.clone(),
                    display_name: if h.is_local {
                        "This Mac".to_string()
                    } else {
                        h.alias.clone()
                    },
                    uri: h.uri.clone(),
                    is_local: h.is_local,
                    color: h.color.clone(),
                    status: h.status.clone(),
                    snapshot: h.snapshot.clone(),
                })
            })
            .collect()
    }

    /// Active `MuxClient` for `alias`, if connected.
    pub fn client(&self, alias: &str) -> Option<Arc<MuxClient>> {
        self.hosts.lock().get(alias).and_then(|h| h.client.clone())
    }

    /// Synchronously obtain (or immediately connect to) the local Mux client
    /// during app startup so the first window opens with zero delay.
    pub fn connect_local_now(&self) -> Option<Arc<MuxClient>> {
        if let Some(c) = self.client("local") {
            return Some(c);
        }
        let target = Target::Local { socket: default_socket_path() };
        let client = MuxClient::connect(&target, true, "gui", "Tachyon.app").ok()?;
        let snap = client.state().ok();
        let rtt = client.ping(Duration::from_secs(2)).ok().map(|d| d.as_secs_f64() * 1000.0);
        {
            let mut hosts = self.hosts.lock();
            if let Some(h) = hosts.get_mut("local") {
                h.status = HostStatus::Connected { info: client.host.clone(), rtt_ms: rtt };
                h.snapshot = snap;
                h.client = Some(Arc::clone(&client));
            }
        }
        self.notify_change();
        Some(client)
    }

    /// Trigger an immediate refresh/reconnect for `alias`.
    pub fn reconnect(&self, alias: &str) {
        if let Some(h) = self.hosts.lock().get(alias) {
            let _ = h.wake.send(());
        }
    }

    /// Refresh snapshot for `alias` asynchronously (e.g. after a local GUI mutation).
    pub fn refresh_host(self: &Arc<Self>, alias: &str) {
        let Some(client) = self.client(alias) else { return };
        let mgr = Arc::clone(self);
        let alias = alias.to_string();
        let _ = std::thread::Builder::new()
            .name("tachyon-host-refresh".into())
            .spawn(move || {
                if let Ok(snap) = client.state() {
                    if let Some(h) = mgr.hosts.lock().get_mut(&alias) {
                        h.snapshot = Some(snap);
                    }
                    mgr.notify_change();
                }
            });
    }

    /// Add and persist a new remote host (`alias`, `ssh://...` or `tcp://...`) and connect immediately.
    pub fn add_host(
        self: &Arc<Self>,
        alias: String,
        uri: String,
        token: Option<String>,
    ) -> Result<(), String> {
        let alias = alias.trim().to_string();
        let uri = uri.trim().to_string();
        if alias.is_empty() || alias == "local" {
            return Err("invalid host alias".into());
        }
        let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let mut saved = load_hosts();
        let preserved_color = saved
            .iter()
            .find(|e| e.alias == alias)
            .and_then(|e| e.color.clone());
        let entry = HostEntry {
            alias: alias.clone(),
            uri: uri.clone(),
            remote_bin: None,
            token,
            color: preserved_color.clone(),
        };
        let target = resolve_target(Some(&alias), std::slice::from_ref(&entry))
            .map_err(|e| e.to_string())?;
        if let Some(prev) = saved.iter_mut().find(|e| e.alias == alias) {
            *prev = entry;
        } else {
            saved.push(entry);
        }
        save_hosts(&saved).map_err(|e| e.to_string())?;
        self.spawn_host(alias, uri, target, false, preserved_color);
        self.notify_change();
        Ok(())
    }

    /// Update and persist a remote host's accent / danger colour preset (`None` = auto by OS).
    pub fn set_host_color(&self, alias: &str, color: Option<String>) -> Result<(), String> {
        let normalized = color
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("auto"));
        if let Some(ref c) = normalized {
            if !crate::mux::model::HostColor::is_valid_spec(c) {
                return Err(format!("invalid host colour `{c}`"));
            }
        }
        {
            let mut hosts = self.hosts.lock();
            if let Some(h) = hosts.get_mut(alias) {
                h.color = normalized.clone();
            }
        }
        let mut saved = load_hosts();
        if let Some(entry) = saved.iter_mut().find(|e| e.alias == alias) {
            entry.color = normalized;
            save_hosts(&saved).map_err(|e| e.to_string())?;
        }
        self.notify_change();
        Ok(())
    }

    /// Remove a configured remote host (cannot remove `local`).
    pub fn remove_host(&self, alias: &str) -> Result<(), String> {
        if alias == "local" {
            return Err("cannot remove local host".into());
        }
        {
            self.order.lock().retain(|a| a != alias);
            if let Some(prev) = self.hosts.lock().remove(alias) {
                prev.stop.store(true, Ordering::Release);
                let _ = prev.wake.send(());
            }
        }
        let mut saved = load_hosts();
        saved.retain(|e| e.alias != alias);
        let _ = save_hosts(&saved);
        self.notify_change();
        Ok(())
    }
}

fn host_worker(
    mgr: Arc<HostManager>,
    alias: String,
    target: Target,
    is_local: bool,
    stop: Arc<AtomicBool>,
    wake_rx: Receiver<()>,
) {
    let mut backoff = Duration::from_secs(1);

    while !stop.load(Ordering::Acquire) {
        {
            let mut hosts = mgr.hosts.lock();
            if let Some(h) = hosts.get_mut(&alias) {
                if !matches!(h.status, HostStatus::Connected { .. }) {
                    h.status = HostStatus::Connecting;
                }
            }
        }
        mgr.notify_change();

        // Reuse an already-connected local client if `connect_local_now` established one.
        let connected = if is_local {
            mgr.client(&alias)
                .filter(|c| !c.is_closed())
                .map(Ok)
                .unwrap_or_else(|| MuxClient::connect(&target, true, "gui", "Tachyon.app"))
        } else {
            MuxClient::connect(&target, false, "gui", "Tachyon.app")
        };

        let client = match connected {
            Ok(c) => {
                backoff = Duration::from_secs(1);
                c
            }
            Err(e) => {
                {
                    let mut hosts = mgr.hosts.lock();
                    if let Some(h) = hosts.get_mut(&alias) {
                        h.status = HostStatus::Disconnected { error: e.to_string() };
                        h.client = None;
                    }
                }
                mgr.notify_change();
                // Wait for backoff or explicit manual reconnect poke.
                let _ = wake_rx.recv_timeout(backoff);
                while wake_rx.try_recv().is_ok() {}
                backoff = (backoff * 2).min(Duration::from_secs(15));
                continue;
            }
        };

        let rtt_ms = client.ping(Duration::from_secs(3)).ok().map(|d| d.as_secs_f64() * 1000.0);
        let snap = client.state().ok();
        let events = client.subscribe_events().ok();

        {
            let mut hosts = mgr.hosts.lock();
            if let Some(h) = hosts.get_mut(&alias) {
                h.status = HostStatus::Connected { info: client.host.clone(), rtt_ms };
                h.snapshot = snap;
                h.client = Some(Arc::clone(&client));
            }
        }
        mgr.notify_change();

        // Pump live server events and 15-second RTT heartbeats until disconnect.
        let mut last_ping = std::time::Instant::now();
        while !stop.load(Ordering::Acquire) && !client.is_closed() {
            // Manual wake triggers an immediate refresh/ping.
            if wake_rx.try_recv().is_ok() {
                while wake_rx.try_recv().is_ok() {}
                if let Ok(snap) = client.state() {
                    if let Some(h) = mgr.hosts.lock().get_mut(&alias) {
                        h.snapshot = Some(snap);
                    }
                    mgr.notify_change();
                }
            }

            if let Some(ref rx) = events {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => {
                        let mut pending = vec![ev];
                        // Coalesce rapid bursts (e.g. session create + initial title).
                        while let Ok(more) = rx.recv_timeout(Duration::from_millis(20)) {
                            pending.push(more);
                        }
                        if let Ok(snap) = client.state() {
                            if let Some(h) = mgr.hosts.lock().get_mut(&alias) {
                                h.snapshot = Some(snap);
                            }
                            mgr.notify_change();
                        }
                        for ev in pending {
                            mgr.notify_event(alias.clone(), ev);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                std::thread::sleep(Duration::from_millis(500));
            }

            if last_ping.elapsed() >= Duration::from_secs(15) {
                last_ping = std::time::Instant::now();
                match client.ping(Duration::from_secs(5)) {
                    Ok(d) => {
                        let ms = d.as_secs_f64() * 1000.0;
                        if let Some(h) = mgr.hosts.lock().get_mut(&alias) {
                            if let HostStatus::Connected { ref mut rtt_ms, .. } = h.status {
                                *rtt_ms = Some(ms);
                            }
                        }
                        mgr.notify_change();
                    }
                    Err(_) => break,
                }
            }
        }

        if stop.load(Ordering::Acquire) {
            break;
        }

        {
            let mut hosts = mgr.hosts.lock();
            if let Some(h) = hosts.get_mut(&alias) {
                h.status = HostStatus::Disconnected { error: "connection lost — reconnecting…".into() };
                h.client = None;
            }
        }
        mgr.notify_change();
        let _ = wake_rx.recv_timeout(backoff);
        while wake_rx.try_recv().is_ok() {}
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}
