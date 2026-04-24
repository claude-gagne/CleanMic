CleanMic v1.0.2 - update notification polish.

A small follow-up to v1.0.1 focused on making the in-app update-notification flow friendlier. No new features; this release tightens the UX around how CleanMic tells you a new version is available.

## What's new in v1.0.2

### Update notification polish

- **Fully localized update banner and desktop notification** - the banner title and notification body are now translated alongside the rest of the UI. French users see French text end-to-end instead of a localized button sitting next to an English headline.
- **Manual "Check for updates" always gives feedback** - clicking "Check for updates" from the tray or hamburger menu now always emits a desktop notification, even if you already dismissed the banner for that version. You get a clear confirmation every time instead of wondering whether the check silently did anything.
- **Tray "Update available" entry opens the download page** - the persistent "Update available: vX.Y.Z" item in the tray menu now opens the GitHub Releases page directly in your browser, mirroring the banner's Download button. Previously it re-ran the update check with no obvious path to actually downloading the new version.

### Under the hood

- Engine selector subtitles ("High quality (default)", "Lightweight, low CPU", "User-supplied, adaptive") are now translatable alongside the rest of the engine picker. Pre-existing gap from v1.0.0, closed here opportunistically.

## Upgrading from v1.0.1

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged. No fields were added, removed, or renamed.
- On launch, v1.0.1 will show an in-app update banner pointing you here.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 22.04+, Fedora 34+)
- GTK4 + libadwaita (standard on GNOME desktops)

## Known Limitations (unchanged from v1.0.1)

- PipeWire only - PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional - requires a user-supplied library, not bundled

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
