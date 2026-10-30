# Glass Island

Glass Island is a small, native Wayland desktop utility that renders a floating
"dynamic island" at the top of the screen. It stays out of the way as a compact
pill and expands to show live system information when clicked or when a
notification arrives.

The application talks to Wayland directly through a
[wlr-layer-shell](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)
overlay surface. It is not an X11 application, an Electron shell, or a
compositor plugin.

## Features

- **Battery** — charge level, charging state, and percentage from UPower over
  the system D-Bus.
- **Now playing** — track title and artist from MPRIS players over the session
  D-Bus.
- **Output volume** — current default-sink volume and mute state from the
  PulseAudio protocol (also used by PipeWire's PulseAudio compatibility layer).
- **Notifications** — a passive monitor of `org.freedesktop.Notifications` that
  expands the island, displays the notification, and collapses again after the
  notification timeout.
- **System monitor** — CPU and memory usage from `/proc`, sampled only while the
  island is expanded.
- **Animated expansion** — an eased compact-to-expanded transition rendered by
  Slint's software renderer.
- **Optional KWin blur** — compositor-side blur behind the island when KWin
  advertises `org_kde_kwin_blur`.
- **Fractional scaling** — uses `wp_fractional_scale_v1` and
  `wp_viewporter` when available, falling back to the output's integer scale.

## Technology stack

| Layer | Technology |
| --- | --- |
| Language | Rust (edition 2024) |
| UI | [Slint](https://slint.dev/) 1.18 with the software renderer |
| Wayland | `wayland-client`, `smithay-client-toolkit`, `wayland-protocols`, `calloop` |
| Desktop services | `zbus` (D-Bus), `pulseaudio` protocol client |
| Async / concurrency | `futures-lite`, `calloop`, std threads |
| Logging | `tracing`, `tracing-subscriber` |

## Architecture

```
src/
├── main.rs                 process startup and service wiring
├── core/
│   ├── state.rs            application data model
│   └── event_bus.rs        typed events delivered through calloop channels
├── platform/
│   ├── wayland.rs          layer-shell host, wl_shm buffers, Slint platform
│   └── kde.rs              optional org_kde_kwin_blur integration
├── services/
│   ├── battery.rs          UPower / D-Bus
│   ├── media.rs            MPRIS / D-Bus
│   ├── notifications.rs    passive notification monitor
│   ├── volume.rs           PulseAudio protocol client
│   └── system_monitor.rs   /proc sampling gated by expansion state
└── ui/
    └── mod.rs              Slint component bindings
ui/
└── island.slint            island layout and animation properties
```

At startup, `main.rs` creates `WaylandRuntime`, installs a Slint platform that
shares the runtime's event loop, builds the `Island` component, and spawns the
service threads. Services never touch the UI directly. They emit `AppEvent`
values over a `calloop` channel; the Wayland host applies each event to shared
state and to the Slint properties on the event-loop thread.

The dedicated surface is a `zwlr_layer_surface_v1` in the `Overlay` layer,
anchored to the top edge with an exclusive zone of `-1` so it does not reserve
screen space. The host surface is a fixed 420×180 logical region; only the pill
is drawn into a transparent `wl_shm` ARGB8888 buffer. Input and blur regions are
set to the rounded pill geometry, so areas outside the pill are neither
clickable nor blurred. Keyboard interactivity is disabled.

## Supported environment

Glass Island requires:

- Linux with a **Wayland session**.
- A compositor that implements `zwlr_layer_shell_v1`.
- A Rust toolchain with edition 2024 support (Rust 1.85 or newer).
- A session D-Bus and, for battery data, a running UPower service.
- PulseAudio or PipeWire's PulseAudio compatibility layer for volume data.

### KDE Plasma and Wayland

The application is developed and tested for **KDE Plasma on Wayland (KWin)**,
which provides wlr-layer-shell through KWin. Compositor blur is a KDE-specific
enhancement and is optional; when `org_kde_kwin_blur` is unavailable the island
still renders with its own translucent gradients.

**GNOME / Mutter is not supported**, because Mutter does not implement
`zwlr_layer_shell_v1`. Other wlroots-based compositors may work but are not
tested or claimed as supported. Do not expect the utility to run under X11.

## Build

A standard Rust toolchain is sufficient; there is no separate system SDK to
install for the Wayland client libraries used here.

```bash
cargo build            # debug build
cargo build --release  # optimized release build
```

The release binary is written to `target/release/glass-island`.

## Run

Start Glass Island from inside a supported Wayland session:

```bash
cargo run --release
# or
./target/release/glass-island
```

Click the pill to expand it, click it again to collapse it, and use the close
button in the expanded view to exit. `SIGINT` and `SIGTERM` also shut it down.

### Environment variables

| Variable | Purpose |
| --- | --- |
| `GLASS_ISLAND_DISABLE_BLUR=1` | Disable the optional KWin blur integration. |
| `GLASS_ISLAND_MONITOR_INTERVAL_MS=<ms>` | System-monitor sample interval, clamped to 500–10000 ms (default 2000). |
| `GLASS_ISLAND_DEMO=1` | Debug builds only: automatically toggles the island shortly after startup. |

## Release build

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build --release
```

The release profile uses thin LTO, a single codegen unit, and strips symbols.

## Current limitations

- The host surface is fixed at 420×180 logical pixels and is positioned on the
  top edge of the output selected at startup. Multi-monitor repositioning at
  runtime is not implemented.
- Interaction is pointer-only; the layer surface requests no keyboard input.
- Notifications are observed passively. Glass Island does not implement
  `org.freedesktop.Notifications` and does not replace the desktop's
  notification daemon.
- MPRIS support selects a single "best" player rather than showing every
  player.
- Volume is display-only; the island does not change the sink volume or mute
  state.
- Battery data depends on UPower exposing a present battery or display device.
- KWin blur is best-effort and depends on the compositor advertising the
  protocol and its blur effect being enabled.
- There is no configuration file, settings UI, autostart entry, or packaging.
- Single-instance behavior is not enforced.

## Development status

Glass Island is in early development (`0.1.0`). The current implementation is a
working native Wayland prototype: the layer-shell host, software-rendered Slint
UI, and the battery, media, volume, notification, and system-monitor services
are functional. APIs and UI details may change before a stable release.

## License

No license file is currently included. Add one before redistributing the
project.
