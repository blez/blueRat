# blueRat

![blueRat banner](assets/banner.png)

**blueRat** is a TUI Bluetooth manager for Linux built with [ratatui](https://ratatui.rs/).
It wraps `bluetoothctl` and `pactl`, so there are no D-Bus bindings and no daemon —
just the tools you already have, with a fast keyboard-driven interface on top.

## Features

- Paired-device list with live connected / trusted state, battery level,
  connected devices first — auto-refreshed every few seconds
- Enter toggles connect / disconnect
- Live scan & pair: discovered devices stream into the picker in real time,
  including devices that need passkey confirmation or PIN entry
- Trust / untrust and remove (with confirmation)
- Automatic audio routing: a freshly connected audio device becomes the
  default sink and existing streams move to it (PipeWire and PulseAudio)
- Audio profile toggle: switch a headset between A2DP (high quality) and
  HFP/HSP (microphone enabled)
- Fuzzy filter, device details popup, banner-style event log
- All blocking operations run on a worker thread — the UI never freezes

## Keys

| Key                    | Action                                                        |
|------------------------|---------------------------------------------------------------|
| `j` / `k` / arrows     | navigate                                                      |
| `Enter`                | connect / disconnect (in scan picker: pair + trust + connect) |
| `s`                    | scan & pair new device (live picker)                          |
| `a`                    | toggle audio profile (A2DP ↔ headset/mic)                     |
| `i`                    | device details popup (`j`/`k` scroll)                         |
| `/`                    | fuzzy filter the device list                                  |
| `t`                    | toggle trust                                                  |
| `x` / `Del`            | remove device (confirmation)                                  |
| `r`                    | refresh                                                       |
| `q` / `Esc` / `Ctrl-C` | quit / back (Esc clears the filter first)                     |

## Requirements

- `bluetoothctl` (bluez) — required
- `pactl` (pulseaudio-utils / pipewire-pulse) — optional, for audio routing

## Build & install

```sh
make release          # build target/release/bluerat
make install          # install binary, icon and .desktop to ~/.local
make PREFIX=/usr/local install   # system-wide instead
make test             # unit tests
```
