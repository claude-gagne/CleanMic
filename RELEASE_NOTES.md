CleanMic v1.0.1 - polish + safety bundle.

A small follow-up to v1.0.0 focused on making the mic picker behave correctly around system-default changes and hot-plug events. No new features; this release tightens the edges of the v1.0.0 core.

## What's new in v1.0.1

### Mic picker + self-loop prevention

- **"Default (MicName)" label** - when your OS default mic is a real physical device, CleanMic surfaces it at the top of the picker as `Default (Razer Seiren X)` or similar, so you can tell which mic "follow system default" resolves to without leaving the app.
- **Self-loop prevention** - when CleanMic itself is set as the OS default input, the "Default" entry is hidden from the picker entirely. The app silently falls back to a real physical microphone for capture, so CleanMic never ends up trying to capture from its own virtual source.
- **Hot-unplug handling** - unplug your selected mic mid-session and CleanMic switches cleanly to the next available physical mic without freezing the input meter or breaking audio to the app that's consuming the virtual source. The picker refreshes live.
- **Hot-replug return** - when you plug your originally-selected mic back in, CleanMic auto-switches capture back to it. Your explicit picks are persisted and honored; auto-switches never silently overwrite them.
- **"No input device available" state** - when no physical mic is enumerable, the picker collapses to a single clearly-labeled entry and the Enable toggle grays out instead of silently spinning the pipeline on no input.

### Under the hood

- Quieter logging - the per-tick device enumeration log is now at debug level rather than info, so running CleanMic from a terminal no longer floods stdout with routine polling output.
- `config.input_device` is now strictly write-through-explicit-user-clicks only; transient auto-switches between mics never rewrite your stored preference.

## Upgrading from v1.0.0

- Your existing config at `~/.config/cleanmic/config.toml` loads unchanged. No fields were added, removed, or renamed. Only the behavior around your stored mic preference tightens slightly (see "Under the hood" above).
- On first launch of v1.0.1, the in-app update banner on older installs will point you here.

## System Requirements

- Linux x86_64 with PipeWire (Ubuntu 22.04+, Fedora 34+)
- GTK4 + libadwaita (standard on GNOME desktops)

## Known Limitations (unchanged from v1.0.0)

- PipeWire only - PulseAudio is not supported
- Linux x86_64 only
- Khip engine is optional - requires a user-supplied library, not bundled

## Support

If CleanMic is useful to you, you can [buy me a coffee](https://buymeacoffee.com/claudegagne).
