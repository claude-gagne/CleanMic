<p align="center">
  <img src="assets/icons/com.cleanmic.CleanMic.svg" width="96" alt="CleanMic icon">
</p>

<h1 align="center">CleanMic</h1>

<p align="center">Noise-free virtual microphone for Linux. It's dead simple: select your mic, enable CleanMic, and every app on your system hears clean audio. Enable it and forget about it.</p>

<p align="center">
  <img src="assets/screenshot.png" alt="CleanMic main window">
</p>

## Features

- **Three noise suppression engines** with pre-tuned defaults:
  - **DeepFilterNet** (default) - modern neural model, high-quality output
  - **RNNoise** - lightweight classic RNN denoiser, low CPU
  - **Khip** - adaptive model (user-supplied library)
- **Light / Balanced / Strong strength dropdown** - tuned per-engine against real noise (fan, keyboard, mouse); each step is a distinct audible change on all three engines
- Works with any app through a PipeWire virtual microphone source (Teams, Meet, Discord, Zoom)
- System tray integration with quick enable / disable
- Monitor - route processed mic back to your headphones when you want to hear what the app hears

## Download

1. Go to [**Releases**](https://github.com/claude-gagne/CleanMic/releases/latest)
2. Download `CleanMic-x86_64.AppImage`
3. Make it executable and run:

    ```bash
    chmod +x CleanMic-x86_64.AppImage
    ./CleanMic-x86_64.AppImage
    ```

## System Requirements

- x86_64 Linux with **PipeWire** and **glibc ≥ 2.39**
- **GTK4 + libadwaita** (standard on GNOME; install `libadwaita-1-0` on KDE / XFCE / Cinnamon desktops)

**Tested on:** Ubuntu 24.04 LTS.

**Should also work on Ubuntu 24.04+ flavors** (same base, not directly tested): Kubuntu, Xubuntu, Ubuntu MATE, Pop!_OS, Linux Mint 22, elementary OS 8, KDE Neon.

Other modern Linux distros (Fedora 40+, Debian 13, Bazzite, Arch, openSUSE Tumbleweed, etc.) with glibc ≥ 2.39, PipeWire, GTK4 and libadwaita should also work — untested from my end. Feedback welcome.

**Won't run on** glibc < 2.39 — including Ubuntu 22.04, Mint 21.x, Pop!_OS 22.04, Fedora ≤ 39, Debian 12, and RHEL / Alma / Rocky 9.

## Known Limitations

- **PipeWire only** - PulseAudio is not supported
- **Linux only** - no Windows or macOS
- **Khip engine is optional** - requires a user-supplied library, not bundled

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

CleanMic is built in the hours around a day job. If it helps you out, you can [buy me a coffee](https://buymeacoffee.com/claudegagne) to help keep it maintained.

No paywalled features. No ads. No nagware in the app. Ever.

## License

MIT
