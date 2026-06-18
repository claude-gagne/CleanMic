## What's new in v1.0.7

### Bug fixes

- **The "Update available" tray entry is now translated.** On a French desktop, the tray menu's update indicator showed the English "Update available: v…" while every other menu item was in French. It now reads "Mise à jour disponible : v…" — the translation already existed and is simply applied correctly.

### Under the hood

- Added an automated translation guard (a pre-commit hook plus a pull-request check) that blocks any new user-facing text from shipping unwrapped or untranslated, so this class of mixed-language glitch can't slip through again.

## Upgrading from v1.0.6

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged.
- No action needed — the fix applies automatically on a French-locale system.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 24.04 LTS, Ubuntu 26.04 LTS, Fedora 44 — best effort on other modern distros, see README)
- GTK4 + libadwaita
- The AppImage requires libfuse3

## Known Limitations (unchanged from v1.0.6)

- PipeWire only — PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional — requires a user-supplied library, not bundled. See "Using Khip" in the README.

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
