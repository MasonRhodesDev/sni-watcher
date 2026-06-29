//! A standalone `org.kde.StatusNotifierWatcher` daemon.
//!
//! Why this exists: when a status bar (e.g. Waybar) hosts the watcher in-process,
//! anything that kills or freezes the bar — `hyprctl reload` does both — takes the
//! tray registry down with it. Bars recover by restarting, which rebuilds an empty
//! registry; well-behaved apps re-register, but Electron apps (Slack, Discord, ...)
//! register exactly once and never come back until relaunched.
//!
//! Hosting the watcher in this separate, headless, Wayland-less process decouples the
//! registry from the bar's lifecycle: the bar can be restarted freely and just
//! re-attaches as a host, reading the still-intact registry. Nothing has to re-register.
//!
//! Spec: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/StatusNotifierWatcher/

use std::sync::Mutex;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::names::WellKnownName;
use zbus::object_server::SignalEmitter;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

/// A single registration: the advertised `service+path` string that hosts consume,
/// plus the unique bus name we watch for disappearance so we can clean it up.
#[derive(Clone)]
struct Registration {
    /// e.g. `:1.234/StatusNotifierItem` or `org.kde.StatusNotifierItem-42-1/StatusNotifierItem`
    entry: String,
    /// unique bus name of the registering connection (`:1.234`)
    owner: String,
}

#[derive(Default)]
struct Watcher {
    items: Mutex<Vec<Registration>>,
    hosts: Mutex<Vec<Registration>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// An item (or its menu) asks to be tracked. `service` is either a bus name
    /// (item lives at the default path) or an object path (item lives on the sender).
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();

        let (advertised_service, path) = if service.starts_with('/') {
            // sender's connection hosts the item at the given path
            (sender.clone(), service.to_string())
        } else {
            // `service` is a bus name; item lives at the well-known path
            (service.to_string(), DEFAULT_ITEM_PATH.to_string())
        };

        let entry = format!("{advertised_service}{path}");
        let owner = if sender.is_empty() {
            advertised_service
        } else {
            sender
        };

        {
            let mut items = self.items.lock().unwrap();
            if items.iter().any(|r| r.entry == entry) {
                tracing::debug!(%entry, "item already registered, ignoring");
                return;
            }
            items.push(Registration {
                entry: entry.clone(),
                owner,
            });
        }

        tracing::info!(%entry, "item registered");
        let _ = Self::status_notifier_item_registered(&emitter, &entry).await;
    }

    /// A host (the bar) announces itself. Items often wait for a host before showing.
    async fn register_status_notifier_host(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let owner = if sender.is_empty() {
            service.to_string()
        } else {
            sender
        };

        {
            let mut hosts = self.hosts.lock().unwrap();
            if hosts.iter().any(|r| r.entry == service) {
                return;
            }
            hosts.push(Registration {
                entry: service.to_string(),
                owner,
            });
        }

        tracing::info!(host = %service, "host registered");
        let _ = Self::status_notifier_host_registered(&emitter).await;
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.entry.clone())
            .collect()
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        !self.hosts.lock().unwrap().is_empty()
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_unregistered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Drop every registration owned by a bus name that just vanished, emitting the
/// appropriate unregistration signals. This is what keeps the registry honest when
/// an app actually exits (as opposed to the bar merely restarting).
async fn handle_name_lost(
    iface_ref: &zbus::object_server::InterfaceRef<Watcher>,
    gone: &str,
) -> zbus::Result<()> {
    let (removed_items, host_now_empty) = {
        let iface = iface_ref.get().await;

        let mut removed_items = Vec::new();
        {
            let mut items = iface.items.lock().unwrap();
            items.retain(|r| {
                if r.owner == gone {
                    removed_items.push(r.entry.clone());
                    false
                } else {
                    true
                }
            });
        }

        let host_now_empty = {
            let mut hosts = iface.hosts.lock().unwrap();
            let had_hosts = !hosts.is_empty();
            hosts.retain(|r| r.owner != gone);
            had_hosts && hosts.is_empty()
        };

        (removed_items, host_now_empty)
    };

    let emitter = iface_ref.signal_emitter();
    for entry in &removed_items {
        tracing::info!(%entry, "item unregistered (owner left the bus)");
        Watcher::status_notifier_item_unregistered(emitter, entry).await?;
    }
    if host_now_empty {
        tracing::info!("last host left the bus");
        Watcher::status_notifier_host_unregistered(emitter).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let name = WellKnownName::try_from(WATCHER_NAME).context("invalid well-known name")?;

    // Serve the object first, then claim the name, so we answer correctly the instant
    // anyone notices us.
    let conn = zbus::connection::Builder::session()
        .context("failed to connect to the session bus")?
        .serve_at(WATCHER_PATH, Watcher::default())
        .context("failed to register the watcher object")?
        .build()
        .await
        .context("failed to build the D-Bus connection")?;

    let reply = conn
        .request_name_with_flags(
            name,
            RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue,
        )
        .await
        .context("RequestName failed")?;

    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
            tracing::info!("owning {WATCHER_NAME}");
        }
        other => {
            anyhow::bail!(
                "could not become the primary owner of {WATCHER_NAME} ({other:?}); \
                 another watcher is already running and would not yield the name"
            );
        }
    }

    // Watch the bus for connections dropping so we can evict their registrations.
    let dbus = DBusProxy::new(&conn)
        .await
        .context("failed to create the org.freedesktop.DBus proxy")?;
    let mut name_changes = dbus
        .receive_name_owner_changed()
        .await
        .context("failed to subscribe to NameOwnerChanged")?;

    let iface_ref = conn
        .object_server()
        .interface::<_, Watcher>(WATCHER_PATH)
        .await
        .context("failed to look up the served watcher interface")?;

    tracing::info!("ready");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
            Some(signal) = name_changes.next() => {
                let args = match signal.args() {
                    Ok(args) => args,
                    Err(err) => {
                        tracing::warn!(%err, "could not parse NameOwnerChanged");
                        continue;
                    }
                };
                // A non-empty new owner means the name was *acquired*, not lost.
                if args.new_owner.is_some() {
                    continue;
                }
                let gone = args.name.to_string();
                if let Err(err) = handle_name_lost(&iface_ref, &gone).await {
                    tracing::warn!(%err, name = %gone, "failed to handle a lost name");
                }
            }
        }
    }

    Ok(())
}
