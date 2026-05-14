## What's new in v1.0.5

### Bug fixes

- **Autostart toggle now correctly reflects whether CleanMic actually starts on login.** Previously, the toggle could show "off" while a stale autostart entry from an earlier session continued to fire every login. v1.0.5 detects this drift on startup and reconciles the system state to match what the toggle says.

## Upgrading from v1.0.4

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged.
- If you previously experienced the autostart-toggle drift (CleanMic starting at login despite the toggle being off), upgrading to v1.0.5 cleans up the stale entry on first launch.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 24.04 LTS, Ubuntu 26.04 LTS, Fedora 44 — best effort on other modern distros, see README)
- GTK4 + libadwaita
- The AppImage requires libfuse3

## Known Limitations (unchanged from v1.0.4)

- PipeWire only — PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional — requires a user-supplied library, not bundled. See "Using Khip" in the README.

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
