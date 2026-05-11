## What's new in v1.0.4

### Fedora support

- **Khip auto-discovery now covers `/usr/lib64` and `/usr/local/lib64`**, the standard system-wide library directories on Fedora, openSUSE, RHEL, Alma, and Rocky. Dropping `libkhip.so` into either path now works as expected.
- **Confirmed working on Fedora 44** (GNOME on Wayland).

### Autostart UX

- **CleanMic now detects whether your desktop has a system tray and adapts.** On desktops without an AppIndicator extension (default GNOME on Fedora, Ubuntu, and others), the autostart flow launches the window minimized to the taskbar instead of trying to hide it.

### Window controls

- **New header-bar minimize button.**
- **New hamburger menu** with "Report an issue" so you can file bugs straight from the app.

## Upgrading from v1.0.3

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged.
- If you had `libkhip.so` installed in `~/.local/lib/` or `/usr/lib/` for v1.0.3, you can leave it there — those paths are still searched. The new `/usr/lib64` and `/usr/local/lib64` entries are additive.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 24.04 LTS, Ubuntu 26.04 LTS, Fedora 44 — best effort on other modern distros, see README)
- GTK4 + libadwaita
- The AppImage requires libfuse3

## Known Limitations (unchanged from v1.0.3)

- PipeWire only — PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional — requires a user-supplied library, not bundled. See "Using Khip" in the README.
