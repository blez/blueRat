# blueRat

![blueRat banner](assets/banner.png)

**blueRat** is a TUI Bluetooth manager for Linux built with [ratatui](https://ratatui.rs/).
It wraps `bluetoothctl` and `pactl`, so there are no D-Bus bindings and no daemon —
just the tools you already have, with a fast keyboard-driven interface on top.

## Features

- Paired-device list with live connected / trusted state
- Enter toggles connect / disconnect
- Scan & pair new devices (10s scan, picker of discovered devices)
- Trust / untrust and remove (with confirmation)
- Automatic audio routing: a freshly connected audio device becomes the
  default sink and existing streams move to it (PipeWire and PulseAudio)
- All blocking operations run on a worker thread — the UI never freezes

## Keys

| Key                    | Action                                                        |
|------------------------|---------------------------------------------------------------|
| `j` / `k` / arrows     | navigate                                                      |
| `Enter`                | connect / disconnect (in scan picker: pair + trust + connect) |
| `s`                    | scan & pair new device                                        |
| `t`                    | toggle trust                                                  |
| `x` / `Del`            | remove device (confirmation)                                  |
| `r`                    | refresh                                                       |
| `q` / `Esc` / `Ctrl-C` | quit / back                                                   |

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

`assets/icon.png` is the icon master (512px, transparent background);
`assets/icon-256.png` is the launcher-size derivative that `make install`
places into the hicolor theme.
