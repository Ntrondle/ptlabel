# PTLabel

Fast native macOS label app (Rust + egui) for Brother **PT-P710BT** (Bluetooth) and **PT-E550W** (Wi-Fi). Also speaks to the PT-P750W, which uses the same protocol. Protocol notes and the plan are in `docs/PLAN.md`; Brother's raster spec is in `docs/`.

## Build / install

```sh
scripts/bundle.sh --install     # -> ~/Applications/PTLabel.app
scripts/bundle.sh --dmg         # -> target/PTLabel.dmg (~3.2 MB, drag to Applications)
python3 scripts/make_icon.py    # regenerate assets/ icon (Pillow; pngquant shrinks it)
cargo run                       # dev
cargo test
```

## Connecting

- **PT-E550W (Wi-Fi):** found automatically over Bonjour (`_pdl-datastream._tcp`), or type its IP in the top bar. Print data goes over raw TCP 9100. Status and tape detection come from SNMP, because the printer doesn't answer status requests on 9100.
- **PT-P710BT (Bluetooth):** pair it once in System Settings › Bluetooth. macOS then creates `/dev/cu.PT-P710BT…`, which shows up in the printer menu.

## Features

- Live tape detection: width, laminated/non-laminated/heat-shrink, tape and ink colour. The layout and preview follow the loaded cassette. Polling runs every 2 s (Wi-Fi) or 3 s (Bluetooth).
- Pixel-exact preview at 180 dpi, drawn in the cassette's colours.
- Blocks laid out left to right: multi-line text (any installed font, auto-fit or pt size, B/I/U, alignment, line spacing), QR, Data Matrix (square or rectangular for narrow tape), Code 128/39, EAN-13/8, UPC-A, images (threshold or dither), and spacers.
- Label options: auto or fixed length, alignment, gap, frame, invert.
- Labels 1…99 999 or ∞ (until Stop or tape-out). Stop finishes the current label and cuts it.
- **Sequence** block: prefix + number (start, step, zero-padded digits) + suffix; prints `count` labels. Its value is also `{seq}` for QR/barcodes.
- **Excel** (xlsx/xlsm/xlsb/xls/ods/csv): open a sheet, tick rows (click, ⇧-click range, search), one label per row. Columns go on the label as `{Column}` (the **+ Field** menu inserts them). The file reloads automatically when you save it. Try `examples/parts.xlsx`.
- Placeholders: `{n}` / `{n:03}` (start + step per label), `{seq}`, `{date}`, `{time}`, `{Column}`.
- Cutting: auto cut; half cut and cut every N (E550W only, greyed out on the P710BT); chain printing; never cut (special tape).
- Output: high resolution 180×360, mirror, margins 2–127 mm.
- Save and open designs as `.ptlabel` JSON. ⌘P print, ⌘S save, ⌘O open, ⌘N new.

## CLI

```sh
ptlabel list
ptlabel status 192.168.1.160
ptlabel render --tape 24 --label examples/asset-tag.ptlabel --out tag.png
ptlabel print 192.168.1.160 --copies 3 --half-cut 'Hello\nworld'
ptlabel print /dev/cu.PT-P710BT6431 --label examples/asset-tag.ptlabel
```
