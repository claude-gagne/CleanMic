## What's new in v1.0.6

### Bug fixes

- **No more spurious "no system tray detected" notification at login.** When CleanMic started automatically on login, it sometimes checked for the system tray a moment before your desktop's tray support (the AppIndicator extension on GNOME) had finished loading. CleanMic would then wrongly conclude there was no tray, fall back to its no-tray behaviour, and show a "no notification area detected" message — even though your tray was working fine. v1.0.6 retries the check for a few seconds, so the tray is found as soon as it appears and the false warning no longer fires.

## Upgrading from v1.0.5

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged.
- If you saw the occasional "no system tray detected" notification on login despite having a working tray, upgrading to v1.0.6 resolves it — no action needed.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 24.04 LTS, Ubuntu 26.04 LTS, Fedora 44 — best effort on other modern distros, see README)
- GTK4 + libadwaita
- The AppImage requires libfuse3

## Known Limitations (unchanged from v1.0.5)

- PipeWire only — PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional — requires a user-supplied library, not bundled. See "Using Khip" in the README.

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
