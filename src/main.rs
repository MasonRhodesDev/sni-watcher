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

impl Registration {
    /// Resolve a `RegisterStatusNotifierItem` argument into a registration.
    /// `service` is either an object path (the item lives on the sender's
    /// connection) or a bus name (the item lives at the well-known path).
    fn for_item(service: &str, sender: &str) -> Registration {
        let (advertised_service, path) = if service.starts_with('/') {
            // sender's connection hosts the item at the given path
            (sender.to_string(), service.to_string())
        } else {
            // `service` is a bus name; item lives at the well-known path
            (service.to_string(), DEFAULT_ITEM_PATH.to_string())
        };

        let entry = format!("{advertised_service}{path}");
        let owner = if sender.is_empty() {
            advertised_service
        } else {
            sender.to_string()
        };
        Registration { entry, owner }
    }
}

#[derive(Default)]
struct Watcher {
    items: Mutex<Vec<Registration>>,
    hosts: Mutex<Vec<Registration>>,
}

impl Watcher {
    /// Track an item registration. Returns `false` (and changes nothing) when
    /// the exact entry is already registered.
    fn add_item(&self, registration: Registration) -> bool {
        let mut items = self.items.lock().unwrap();
        if items.iter().any(|r| r.entry == registration.entry) {
            return false;
        }
        items.push(registration);
        true
    }

    /// Track a host registration. Returns `false` when already registered.
    fn add_host(&self, registration: Registration) -> bool {
        let mut hosts = self.hosts.lock().unwrap();
        if hosts.iter().any(|r| r.entry == registration.entry) {
            return false;
        }
        hosts.push(registration);
        true
    }

    /// Drop every registration owned by a bus name that just vanished.
    /// Returns the removed item entries and whether the last host just left.
    fn evict_owner(&self, gone: &str) -> (Vec<String>, bool) {
        let mut removed_items = Vec::new();
        {
            let mut items = self.items.lock().unwrap();
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
            let mut hosts = self.hosts.lock().unwrap();
            let had_hosts = !hosts.is_empty();
            hosts.retain(|r| r.owner != gone);
            had_hosts && hosts.is_empty()
        };

        (removed_items, host_now_empty)
    }
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
        let registration = Registration::for_item(service, &sender);
        let entry = registration.entry.clone();

        if !self.add_item(registration) {
            tracing::debug!(%entry, "item already registered, ignoring");
            return;
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

        if !self.add_host(Registration {
            entry: service.to_string(),
            owner,
        }) {
            return;
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
    let (removed_items, host_now_empty) = iface_ref.get().await.evict_owner(gone);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(entry: &str, owner: &str) -> Registration {
        Registration {
            entry: entry.to_string(),
            owner: owner.to_string(),
        }
    }

    #[test]
    fn for_item_object_path_binds_to_sender() {
        // Item lives on the sender's connection at the given path.
        let r = Registration::for_item("/StatusNotifierItem", ":1.42");
        assert_eq!(r.entry, ":1.42/StatusNotifierItem");
        assert_eq!(r.owner, ":1.42");
    }

    #[test]
    fn for_item_bus_name_uses_default_path() {
        // `service` is a bus name; item lives at the well-known path.
        let r = Registration::for_item("org.kde.StatusNotifierItem-42-1", ":1.7");
        assert_eq!(
            r.entry,
            "org.kde.StatusNotifierItem-42-1/StatusNotifierItem"
        );
        assert_eq!(r.owner, ":1.7");
    }

    #[test]
    fn for_item_missing_sender_falls_back_to_service() {
        let r = Registration::for_item("org.kde.StatusNotifierItem-9-1", "");
        assert_eq!(r.entry, "org.kde.StatusNotifierItem-9-1/StatusNotifierItem");
        assert_eq!(r.owner, "org.kde.StatusNotifierItem-9-1");
    }

    #[test]
    fn add_item_deduplicates_by_entry() {
        let w = Watcher::default();
        assert!(w.add_item(reg(":1.5/StatusNotifierItem", ":1.5")));
        // Same entry again (even from another owner) is a no-op.
        assert!(!w.add_item(reg(":1.5/StatusNotifierItem", ":1.6")));
        assert_eq!(w.items.lock().unwrap().len(), 1);
    }

    #[test]
    fn add_host_deduplicates_by_entry() {
        let w = Watcher::default();
        assert!(w.add_host(reg("org.kde.StatusNotifierHost-waybar", ":1.9")));
        assert!(!w.add_host(reg("org.kde.StatusNotifierHost-waybar", ":1.9")));
        assert_eq!(w.hosts.lock().unwrap().len(), 1);
    }

    #[test]
    fn evict_owner_removes_only_that_owners_items() {
        let w = Watcher::default();
        w.add_item(reg(":1.5/StatusNotifierItem", ":1.5"));
        w.add_item(reg(":1.5/OtherItem", ":1.5"));
        w.add_item(reg(":1.8/StatusNotifierItem", ":1.8"));

        let (removed, host_now_empty) = w.evict_owner(":1.5");
        assert_eq!(removed, vec![":1.5/StatusNotifierItem", ":1.5/OtherItem"]);
        assert!(!host_now_empty, "no hosts were registered at all");
        assert_eq!(w.items.lock().unwrap().len(), 1);
        assert_eq!(w.items.lock().unwrap()[0].entry, ":1.8/StatusNotifierItem");
    }

    #[test]
    fn evict_owner_reports_when_last_host_leaves() {
        let w = Watcher::default();
        w.add_host(reg("org.kde.StatusNotifierHost-waybar", ":1.9"));

        // An unrelated name vanishing doesn't count as losing the last host.
        let (removed, host_now_empty) = w.evict_owner(":1.3");
        assert!(removed.is_empty());
        assert!(!host_now_empty);

        // The actual host leaving does.
        let (removed, host_now_empty) = w.evict_owner(":1.9");
        assert!(removed.is_empty());
        assert!(host_now_empty);

        // And with zero hosts left, another eviction is not a transition.
        let (_, host_now_empty) = w.evict_owner(":1.9");
        assert!(!host_now_empty);
    }

    #[test]
    fn evict_unknown_owner_is_a_noop() {
        let w = Watcher::default();
        w.add_item(reg(":1.5/StatusNotifierItem", ":1.5"));
        let (removed, host_now_empty) = w.evict_owner(":1.999");
        assert!(removed.is_empty());
        assert!(!host_now_empty);
        assert_eq!(w.items.lock().unwrap().len(), 1);
    }
}
