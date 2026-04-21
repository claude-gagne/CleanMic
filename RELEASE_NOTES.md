CleanMic v1.0.0 - a noise-free virtual microphone for Linux.

CleanMic is a small desktop app for Ubuntu, Fedora, and other modern Linux distributions. Select your physical microphone, enable CleanMic, and every app on your system - Teams, Meet, Discord, Zoom, OBS - hears clean audio through a single PipeWire virtual source. It feels like a system utility: enable it and forget about it.

## What's in v1.0.0

- **Three noise suppression engines, pre-tuned out of the box** - pick the one you like; no fiddling with DSP parameters:
  - **DeepFilterNet** (default) - modern neural model, high-quality output with no robotic artefacts
  - **RNNoise** - lightweight classic RNN denoiser, low CPU
  - **Khip** - adaptive model (user-supplied library), locks onto your noise profile over 1–2 s
- **A Low / Medium / High strength slider that actually means something** - tuned per-engine against real noise (fan, keyboard, mouse); each step is a distinct audible change on all three engines, not just a cosmetic label
- **Automatic update check** - quiet in-app banner, desktop notification, and tray indicator when a new release ships; fails silently if offline
- **Stereo monitor** - route processed mic to both ears when you want to hear what the app hears
- **System tray integration** - quick enable / disable without opening the main window
- **French translation included**

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 22.04+, Fedora 34+)
- GTK4 + libadwaita (standard on GNOME desktops)

## Known Limitations

- PipeWire only - PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional - requires a user-supplied library, not bundled

## Support

If CleanMic is useful to you, you can [sponsor on GitHub](https://github.com/sponsors/claude-gagne) or [buy a coffee](https://buymeacoffee.com/claudegagne).
