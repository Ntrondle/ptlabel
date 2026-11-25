<div align="center">

<img src="assets/icon.png" width="72" height="72" alt="PTLabel icon" />

# PTLabel

**Fast native label app & CLI for Brother P-touch printers on macOS and Windows.**

[Download](https://github.com/Ntrondle/ptlabel/releases/latest) · [CLI Reference](docs/CLI.md) · [Protocol Notes](docs/PLAN.md)

<br />

<img src="assets/screenshot.png" alt="PTLabel showing live 180 dpi preview, QR code, and Excel mail-merge" width="100%" />

</div>

## Install

Download the latest release from **[GitHub Releases](https://github.com/Ntrondle/ptlabel/releases/latest)**:

- **macOS (Apple Silicon):** `PTLabel-…-macos-arm64.dmg` — drag to Applications.
- **Windows:** `PTLabel-…-windows-x86_64.exe` — standalone executable, no installer required.

Release builds check for updates automatically and update in place.

## Features

- **Live tape detection & 180 dpi preview** — reads cassette width, type, and tape/ink colours from the printer and renders pixel-exact output.
- **Composable blocks** — multi-line text (any system font, auto-fit or pt size, `0/90/180/270°` rotation), QR, Data Matrix (square or rectangular for narrow tape), Code 128/39, EAN-13/8, UPC-A, images (threshold or dither), sequences, and spacers.
- **Spreadsheet mail-merge** — open `.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.ods`, or `.csv` files by **rows or columns**, pick records to print, insert `{Column}` fields, and auto-reload on save (try [`examples/parts.xlsx`](examples/parts.xlsx)).
- **Templates & sequences** — dynamic `{n}`, `{n:03}`, `{seq}` / `{seq2}`, `{date}`, and `{time}` placeholders across text, QR, and barcodes.
- **Layout & output** — auto or fixed length (with lab slide, microtube, and Falcon presets), frame, invert, high-res `180×360` dpi, mirror, chain printing, half-cut & cut-every-N (PT-E550W), PNG export, and `.ptlabel` JSON files.

## Printers

| Connection | Supported models | Notes |
| --- | --- | --- |
| **Wi-Fi** | PT-E550W, PT-P750W | Auto-discovered over mDNS/Bonjour, or enter an IP address directly. |
| **Bluetooth** | PT-P710BT | Pair once in OS Bluetooth settings — PTLabel connects directly. *(macOS: don't click **Connect** in System Settings, or macOS grabs the printer as a braille display for ~1 min. Windows: no manual COM port setup needed.)* |
| **USB** *(Windows)* | PT-P710BT, PT-E550W, PT-P750W, PT-P700, PT-D600, PT-P900 series, and other P-touch models | Plug-and-play over Windows' built-in `usbprint.sys` driver (no Zadig/WinUSB needed). |

> [!IMPORTANT]
> PTLabel has only been tested on the **PT-E550W** and **PT-P710BT**. Other P-touch models share the same Brother raster protocol and should work, but are untested — please open an issue if you run into any quirks.

## CLI

The app binary doubles as a headless CLI for scripts and agents (`list`, `status`, `render`, `print`). See [`docs/CLI.md`](docs/CLI.md) for the full reference and `.ptlabel` schema.

```sh
ptlabel list
ptlabel status 192.168.1.160
ptlabel render --tape 24 --label examples/asset-tag.ptlabel --out tag.png
ptlabel print 192.168.1.160 --copies 3 --half-cut 'Hello\nworld'
ptlabel print usb --label examples/asset-tag.ptlabel          # Windows USB
ptlabel print /dev/cu.PT-P710BT6431 --label examples/asset-tag.ptlabel
```

## Build from source

Built in Rust with `egui` (Metal on macOS, DX12 on Windows).

```sh
scripts/bundle.sh --install   # macOS: build & install to ~/Applications/PTLabel.app
scripts/bundle.sh --dmg       # macOS: build target/PTLabel.dmg
cargo build --release         # Windows: target/release/ptlabel.exe
cargo test
```
