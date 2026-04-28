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

**Tested on:** Ubuntu 24.04 LTS, Ubuntu 26.04 LTS.

**Should also work on Ubuntu 24.04+ flavors** (same base, not directly tested): Kubuntu, Xubuntu, Ubuntu MATE, Pop!_OS, Linux Mint 22, elementary OS 8, KDE Neon.

Other modern Linux distros (Fedora 40+, Debian 13, Bazzite, Arch, openSUSE Tumbleweed, etc.) with glibc ≥ 2.39, PipeWire, GTK4 and libadwaita should also work — untested from my end. Feedback welcome.

**Won't run on** glibc < 2.39 — including Ubuntu 22.04, Mint 21.x, Pop!_OS 22.04, Fedora ≤ 39, Debian 12, and RHEL / Alma / Rocky 9.

## Known Limitations

- **PipeWire only** - PulseAudio is not supported
- **Linux only** - no Windows or macOS
- **Khip engine is optional** - requires a user-supplied library, not bundled

## Using Khip

The Khip engine is user-supplied — CleanMic does not ship the library
because its license forbids redistribution. To enable Khip:

1. Build Khip from upstream source:

    ```bash
    git clone https://github.com/repository/khip
    cd khip
    meson setup build
    cd build
    ninja
    ```

2. Copy the resulting library into a directory CleanMic searches:

    ```bash
    cp build/libkhip.so ~/.local/lib/
    ```

    CleanMic searches `/usr/lib`, `/usr/lib/x86_64-linux-gnu`,
    `/usr/local/lib`, and `~/.local/lib` — `~/.local/lib` is the
    recommended target because it requires no `sudo` and is
    per-user.

3. CleanMic auto-detects within ~1.5 seconds — no relaunch needed.
   The "Khip (not installed)" row in the engine selector flips to
   plain "Khip" and becomes selectable.

## Troubleshooting

### Khip row stays grayed after copying `libkhip.so`

Run CleanMic with logging enabled and grep for the discovery message:

```bash
RUST_LOG=info ./CleanMic-x86_64.AppImage 2>&1 | grep -i khip
```

The line `Khip library not found in any of: ...` confirms CleanMic
did not see the library — re-check that `libkhip.so` (not
`libkhip.so.0` or a versioned symlink) is at one of the four search
paths: `/usr/lib`, `/usr/lib/x86_64-linux-gnu`, `/usr/local/lib`,
or `~/.local/lib`.

### `deep_filter_ladspa | Underrun detected` warnings at `RUST_LOG=info`

When DeepFilterNet is the active engine and you run with
`RUST_LOG=info`, you may see lines like:

```
WARN  deep_filter_ladspa | Underrun detected (RTF: 1.63). Processing too slow!
INFO  deep_filter_ladspa | Increasing processing latency to 10.0ms
```

This is expected log output from the DeepFilterNet LADSPA
plugin's dynamic-latency-manager, not a CleanMic bug. The plugin
starts at 0ms latency, bumps by 10ms on a single-frame underrun to
self-heal, and retries dropping back down every ~10s until it finds
the lowest sustainable latency for your hardware. Audible impact is
roughly one frame (~10ms) per event — imperceptible on voice calls.
Same upstream behavior since DeepFilterNet v1.0.0.

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
