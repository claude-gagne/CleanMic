<p align="center">
  <img src="assets/icons/com.cleanmic.CleanMic.svg" width="96" alt="CleanMic icon">
</p>

<h1 align="center">CleanMic</h1>

<p align="center">Noise-free virtual microphone for Linux. Select your mic, enable CleanMic, and every app hears clean audio.</p>

<p align="center">
  <img src="assets/screenshot.png" alt="CleanMic main window">
</p>

## Features

- **Three noise suppression engines, pre-tuned out of the box** — pick the one you like; no fiddling with DSP parameters:
  - **DeepFilterNet** (default) — modern neural model, high-quality output with no robotic artefacts
  - **RNNoise** — lightweight classic RNN denoiser, low CPU
  - **Khip** — adaptive model (user-supplied library), locks onto your noise profile over 1–2 s
- **A Low / Medium / High strength slider that actually means something** — tuned per-engine against real noise (fan, keyboard, mouse); each step is a distinct audible change on all three engines, not just a cosmetic label
- **Automatic update check** — quiet in-app banner, desktop notification, and tray indicator when a new release ships; fails silently if offline
- **Stereo monitor** — route processed mic to both ears when you want to hear what the app hears
- Works with any app through a PipeWire virtual microphone source (Teams, Meet, Discord, Zoom)
- System tray integration with quick enable / disable
- French translation included
- Feels like a system utility — enable it and forget about it

## Download

1. Go to [**Releases**](https://github.com/claude-gagne/CleanMic/releases/latest)
2. Download `CleanMic-x86_64.AppImage`
3. Make it executable and run:

```bash
chmod +x CleanMic-x86_64.AppImage
./CleanMic-x86_64.AppImage
```

> Thanks for trying CleanMic. If it's useful to you, you can [sponsor on GitHub](https://github.com/sponsors/claude-gagne) or [buy a coffee](https://buymeacoffee.com/claudegagne).

## System Requirements

- Linux x86_64 with **PipeWire** (Ubuntu 22.04+, Fedora 34+)
- **GTK4 + libadwaita** (installed by default on GNOME desktops)

## Known Limitations

- **PipeWire only** — PulseAudio is not supported
- **Linux only** — no Windows or macOS
- **Khip engine is optional** — requires a user-supplied library, not bundled

## Building from Source

```bash
# Install build dependencies (Ubuntu/Debian)
sudo apt install libgtk-4-dev libadwaita-1-dev libpipewire-0.3-dev pkg-config gettext

# Build
make build

# Build AppImage
make appimage
```

## Support

CleanMic is built in the hours around a day job. If it saves you time on calls, you can help keep it maintained:

- **[GitHub Sponsors](https://github.com/sponsors/claude-gagne)** — monthly or one-time, $3 / $10 / $25 tiers
- **[Buy Me a Coffee](https://buymeacoffee.com/claudegagne)** — one-time $3 per coffee

What the money covers: AppImage build infrastructure, investigating bug reports, testing against new mic hardware, and ongoing maintenance between day-job hours.

No paywalled features. No ads. No nagware in the app. Ever.

## License

MIT
