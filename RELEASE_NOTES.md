CleanMic v1.0.3 - Ubuntu 26.04 compatibility + Khip discovery polish.

A UX and reliability follow-up to v1.0.2, driven by an Ubuntu 26.04 UAT pass.
This is not a feature release: it fixes the things that prevented v1.0.2 from
running cleanly on a fresh 26.04 desktop and tightens the Khip-engine
discovery story so users who supply their own library aren't left guessing.

## What's new in v1.0.3

### Ubuntu 26.04 compatibility

- **AppImage runs on default Ubuntu 26.04 desktops out of the box.** v1.0.3 switches the AppImage runtime to uruntime v0.5.7 (libfuse3-based). v1.0.2 used libfuse2, which Ubuntu 26.04 renamed to `libfuse2t64` and removed from the default desktop install — fresh 26.04 boxes couldn't launch v1.0.2 at all without an apt install. v1.0.3 launches with no extra packages required.
- **Mic picker's "Default (DeviceName)" entry now resolves correctly on Ubuntu 26.04** (PipeWire 1.5.x). v1.0.2 queried only the user-configured GNOME Sound Settings input key, which is unwritten on fresh installs where the user hasn't explicitly picked an input. v1.0.3 falls back to PipeWire's runtime default-source so the picker's "Default" row always appears.
- **Tray menu now renders in the user's locale on first launch.** v1.0.2 cached menu strings before locale binding completed and re-translated only after a state change like clicking "Check for updates". v1.0.3 binds locale at the very top of init so every gettext call sees the user locale from process start.

### Khip discovery polish

- **Engine selector replaced with three radio rows** (one per engine) under a Preferences group. The previous dropdown couldn't enforce the disabled state on the Khip row when the library was missing; the new rows are genuinely unclickable when an engine is unavailable. Matches GNOME Sound Settings.
- **CleanMic now hot-detects the Khip library at runtime** — drop `libkhip.so` into `~/.local/lib/` while the app is running and the Khip engine becomes selectable within ~1-2 seconds. No relaunch needed.
- **Grayed Khip row tells you exactly where to put the library.** When Khip is not detected, the subtitle now reads `Not detected — copy libkhip.so to ~/.local/lib/`. French: `Non détecté — copiez libkhip.so dans ~/.local/lib/`.
- **One-time WARN log line** fires per process when Khip isn't detected, giving the same hint at the log surface for users running with `RUST_LOG=info`.
- **New "Using Khip" section in the README** with the build + install path for users compiling Khip from upstream.

### Behind the scenes

- New "Troubleshooting" section in the README documenting that `deep_filter_ladspa | Underrun detected (RTF: …). Processing too slow!` warnings visible at `RUST_LOG=info` are upstream LADSPA dynamic-latency-manager telemetry, not a CleanMic bug — the plugin self-heals.
- Added an `#[ignore]`-gated PipeWire integration test for the virtual mic ports as a regression guard against future PipeWire-version surprises (run with `cargo test --features pipewire --test pw_integration_test -- --ignored`).

## Upgrading from v1.0.2

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged. No fields were added, removed, or renamed.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 24.04 LTS, Ubuntu 26.04 LTS, Fedora 40+ — best effort on older distros)
- GTK4 + libadwaita (standard on GNOME desktops)
- The AppImage requires libfuse3 (default-shipped on Ubuntu 24.04 and Ubuntu 26.04; `libfuse2` no longer required as of v1.0.3)

## Known Limitations (unchanged from v1.0.2)

- PipeWire only — PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional — requires a user-supplied library, not bundled. See "Using Khip" in the README for build + install steps.

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
