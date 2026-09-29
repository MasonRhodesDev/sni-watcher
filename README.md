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

## The host-registered race (fixed in 0.2.0)

Chromium, and therefore every Electron app, creates its tray icon once. It asks
the bus `NameHasOwner("org.kde.StatusNotifierWatcher")`, then reads the
watcher's `IsStatusNotifierHostRegistered` property, and on `false` from either
it gives up for the lifetime of the process (Chromium's
`status_icon_linux_dbus.cc`, `OnImplInitializationFailed`). It never retries and
never listens for `StatusNotifierHostRegistered`.

The bar is the only host, and the bar restarts. Observed at login on 2026-09-28:

| Time | Event |
|---|---|
| 14:26:39.815 | sni-watcher owns the name |
| 14:26:40.390 | Waybar restarts: last host left the bus |
| 14:26:40.469 | Slack creates its tray icon |
| 14:26:40.614 | Waybar's new host registers |

Slack read `IsStatusNotifierHostRegistered` inside that 224 ms window, got
`false`, and had no tray icon until the next Slack restart.

0.2.0 closes this three ways:

- `IsStatusNotifierHostRegistered` is always `true`, and the watcher no longer
  emits `StatusNotifierHostUnregistered` when the bar restarts. The watcher
  exists to outlive the bar, so it answers for the bar: a host will be here.
  Items registered while no bar runs are shown as soon as one attaches;
  nothing is lost.
- The unit is ordered `Before=xdg-desktop-autostart.target`, so XDG autostart
  apps find the name owned (the `NameHasOwner` half of Chromium's check).
- `dist/org.kde.StatusNotifierWatcher.service` makes the name D-Bus
  activatable (`SystemdService=sni-watcher.service`) for clients that call it
  before the unit is up. Chromium does not, so this is not what fixes Electron
  apps; it covers everything else.

Apps started by the compositor (`exec-once`, `uwsm app`) run outside systemd
ordering. For those, the always-true property is the load-bearing fix.

### Registration strings with a path (fixed in 0.2.0)

Current Chromium, and so Slack since its 2026-09 Electron update, registers
`org.freedesktop.StatusNotifierItem-<pid>-1/StatusNotifierItem/1`: bus name
and object path in one string. 0.1.1 treated anything not starting with `/` as
a bare bus name and appended `/StatusNotifierItem`, so Waybar received
`.../StatusNotifierItem/1/StatusNotifierItem`, logged
`Invalid Status Notifier Item`, and drew nothing. 0.2.0 splits at the first
`/` and keeps both halves as given, which is what Waybar's own watcher does.

## Restarting the watcher (read before you do)

A restart of sni-watcher is itself the hazard this daemon exists to remove.
Chromium/Electron apps re-register on the new name owner, but treat one failed
`RegisterStatusNotifierItem` as fatal for the process lifetime. While Waybar
runs it holds a queued claim on the name, so a restart hands the name to
Waybar's in-process watcher for a moment and then back; observed 2026-09-29,
that round trip lost Slack's icon until Slack restarted. Package upgrades
therefore do not restart the unit (since 0.2.1). To pick up a new binary now:

```sh
systemctl --user stop waybar && systemctl --user restart sni-watcher && systemctl --user start waybar
```

With the bar stopped nothing else claims the name; items re-register against
the new instance and Electron apps keep their icons.

## Install

Packaged install only (Arch / Fedora COPR). The binary is built with `cargo
build --release`; the user unit and preset are installed from `dist/` by the
PKGBUILD / spec.

**Arch** — add the `[mason]` repo to `/etc/pacman.conf`, then install:

```ini
[mason]
# Import the signing key first: https://github.com/MasonRhodesDev/arch-repo#use-it
SigLevel = Required DatabaseRequired
Server = https://masonrhodesdev.github.io/arch-repo/x86_64
```

```sh
sudo pacman -Sy sni-watcher
```

**Fedora**

```sh
sudo dnf copr enable solaris765/sni-watcher
sudo dnf install sni-watcher
```

Then enable the service:

```sh
systemctl --user enable --now sni-watcher.service
```

The package ships a systemd **user** service and a preset that enables it. The unit is
`Type=dbus` with `BusName=org.kde.StatusNotifierWatcher`, so systemd considers it
"started" only once it owns the name. Since 0.3.0 it is wanted by
`graphical-session-pre.target` and needs only `dbus.socket`, so it owns the name
before the compositor session, the bar, and every autostart app exist. No Waybar
config change is needed: its `tray` module auto-detects the existing watcher and
becomes a host, and the freeze-on-reload restart workaround stays harmless to the tray.

Upgrading from 0.2.x, run `systemctl --user reenable sni-watcher.service` once so the
wants link moves from `graphical-session.target` to the new target.

### Registry persistence (since 0.3.0)

Every registration change is mirrored to `$XDG_RUNTIME_DIR/sni-watcher/items`
(one `entry<TAB>owner` line per item, written atomically). At start the daemon
reads it back, keeps the items whose bus connection still exists, serves them,
and re-announces them with `StatusNotifierItemRegistered`. A crash, an
`Restart=always` comeback, or a deliberate restart therefore returns with the
same list, which covers apps that register exactly once. It does not cover an
app whose registration call errored while the name had no owner; see the
restart recipe above for how to avoid that.

## Build / release

Packaging follows the `cargo-rpm-macros` + vendored-deps pattern (spec and
PKGBUILD in `packaging/`, units in `dist/`). Cargo.toml is the version source
of truth; `build-srpm.sh` gates on spec Version == Cargo.toml == Cargo.lock ==
PKGBUILD pkgver.

```sh
# bump the version in Cargo.toml, packaging/sni-watcher.spec, and
# packaging/PKGBUILD (keep them in sync), then:
git tag v0.1.0 && git push --tags
packaging/build-srpm.sh           # SRPM from the tag + vendored cargo deps
packaging/build-srpm.sh --copr    # ...and submit to COPR (solaris765/sni-watcher)
packaging/build-srpm.sh --head    # build an SRPM from HEAD for local testing
```

For a quick local binary (no packaging): `cargo install --path .`.

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
