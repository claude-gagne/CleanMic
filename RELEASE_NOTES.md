CleanMic v1.0.0 - a noise-free virtual microphone for Linux.

CleanMic is a small desktop app for Ubuntu, Fedora, and other modern Linux distributions. It's dead simple: select your physical microphone, enable CleanMic, and every app on your system (Teams, Meet, Discord, Zoom, OBS) hears clean audio through a single PipeWire virtual source. Enable it and forget about it.

## What's in v1.0.0

- **Three noise suppression engines** with pre-tuned defaults:
  - **DeepFilterNet** (default) - modern neural model, high-quality output
  - **RNNoise** - lightweight classic RNN denoiser, low CPU
  - **Khip** - adaptive model (user-supplied library)
- **Light / Balanced / Strong strength dropdown** - tuned per-engine against real noise (fan, keyboard, mouse); each step is a distinct audible change on all three engines
- Works with any app through a PipeWire virtual microphone source
- System tray integration with quick enable / disable
- Monitor - route processed mic back to your headphones when you want to hear what the app hears

## System Requirements

- x86_64 Linux with PipeWire and glibc ≥ 2.39
- GTK4 + libadwaita (standard on GNOME)

Tested on Ubuntu 24.04 LTS (GNOME 46, PipeWire 1.0.5). Should also work on current Ubuntu/Fedora-family distros (Pop!_OS, Linux Mint 22, elementary OS 8, Bazzite, Nobara, Silverblue), Debian 13, and rolling distros (Arch, Manjaro, openSUSE Tumbleweed). Won't run on glibc < 2.39 — including Ubuntu 22.04, Mint 21.x, Fedora ≤ 39, Debian 12, and RHEL/Alma/Rocky 9.

## Known Limitations

- PipeWire only - PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional - requires a user-supplied library, not bundled

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
