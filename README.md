# sni-watcher

A standalone `org.kde.StatusNotifierWatcher` daemon, so the system-tray registry
survives status-bar restarts.

## The problem

The system tray uses the StatusNotifierItem (SNI) protocol, which has three roles:

- **Watcher** (`org.kde.StatusNotifierWatcher`) — the registry of all tray items
- **Host** — whatever displays them (e.g. Waybar's `tray` module)
- **Items** — the apps (Slack, blueman, …)

Waybar hosts the **watcher in-process**. That couples the registry's lifetime to the
bar's. On Hyprland, `hyprctl reload` both *freezes* Waybar and forces a restart of it
(the `hyprland/workspaces` module desyncs otherwise), and every restart rebuilds an
**empty** registry. Well-behaved apps re-register when a new watcher appears; Electron
apps (Slack, Discord, …) register exactly once and never re-register — so they vanish
from the tray until relaunched.

## The fix

Run the watcher as a separate, headless, Wayland-less process. `hyprctl reload` can't
freeze it (no surface) and a bar restart can't kill it. Waybar detects the existing
watcher at startup and attaches as a **host only**; when it restarts it just re-reads
the still-intact registry. Nothing has to re-register, so Slack stays put.

Verified: with this daemon owning the watcher, restarting Waybar — and a full
`hyprctl reload` — leaves the registered-item set completely unchanged.

## Build & install

```sh
cargo install --path .          # -> ~/.cargo/bin/sni-watcher
```

## Run it as a user service (started before Waybar)

Install the unit:

```sh
cp contrib/sni-watcher.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sni-watcher.service
```

`Type=dbus` means systemd considers the service "started" only once it owns
`org.kde.StatusNotifierWatcher` — so anything ordered `After=` it is guaranteed to find
the watcher already present.

Then make Waybar start after it, via a drop-in
(`~/.config/systemd/user/waybar.service.d/sni-watcher.conf`):

```ini
[Unit]
After=sni-watcher.service
Wants=sni-watcher.service
```

```sh
systemctl --user daemon-reload
systemctl --user restart sni-watcher.service waybar.service
```

No Waybar config change is needed — its `tray` module auto-detects the existing watcher
and becomes a host. The freeze-on-reload workaround (restarting Waybar) can stay exactly
as it is; it's now harmless to the tray.

## Verify

```sh
# watcher should be owned by sni-watcher, NOT waybar:
busctl --user list | grep StatusNotifierWatcher
# the registry survives a bar restart:
busctl --user get-property org.kde.StatusNotifierWatcher /StatusNotifierWatcher \
    org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems
systemctl --user restart waybar
# ^ re-run the get-property: the item set is unchanged.
```

## Logging

Logs to stderr (captured by the journal). Control verbosity with `RUST_LOG`, e.g.
`RUST_LOG=debug`. `journalctl --user -u sni-watcher -f`.
