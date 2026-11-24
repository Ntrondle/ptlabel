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

**Updates:** the app checks the repo's latest GitHub release every 6 h (on launch, then 6-hourly). If it's newer, it shows an *Update available* dialog and an *Update to x.y.z…* link in the status bar. **Update now** downloads the DMG, verifies the signature, swaps the app in place and reopens it. A bare `cargo run` binary doesn't check. To release, publish a `vX.Y.Z` GitHub release matching `Cargo.toml`'s version; the workflow attaches the DMG.

## Connecting

- **PT-E550W (Wi-Fi):** found automatically over Bonjour (`_pdl-datastream._tcp`), or type its IP in the top bar. Print data goes over raw TCP 9100. Status and tape detection come from SNMP, because the printer doesn't answer status requests on 9100.
- **PT-P710BT (Bluetooth):** pair it once in System Settings › Bluetooth. macOS then creates `/dev/cu.PT-P710BT…`, which shows up in the printer menu. After that, don't click **Connect** on it in the Bluetooth menu: macOS treats its serial port as a braille display and keeps the printer's only channel for about a minute, so PTLabel gets no replies. Just turn the printer on; PTLabel connects by itself.

## Features

- Live tape detection: width, laminated/non-laminated/heat-shrink, tape and ink colour. The layout and preview follow the loaded cassette. Polling runs every 2 s (Wi-Fi) or 3 s (Bluetooth).
- Pixel-exact preview at 180 dpi, drawn in the cassette's colours.
- Blocks laid out left to right: multi-line text (any installed font, auto-fit or pt size, B/I/U, alignment, line spacing), QR, Data Matrix (square or rectangular for narrow tape), Code 128/39, EAN-13/8, UPC-A, images (threshold or dither), and spacers.
- Text rotation 0/90/180/270° per text or sequence block (↻ button). At 90°/270° the lines run across the tape and auto-fit makes the longest line fill the tape height, which helps on narrow tape.
- Label options: auto or fixed length, alignment, gap, frame, invert.
- Labels 1…99 999 or ∞ (until Stop or tape-out). Stop finishes the current label and cuts it.
- **Sequence** block: prefix + number (start, step, zero-padded digits) + suffix; prints `count` labels. Its value is also `{seq}` for QR/barcodes.
- **Excel** (xlsx/xlsm/xlsb/xls/ods/csv): open a sheet, tick rows (click, ⇧-click range, search), one label per row. Columns go on the label as `{Column}` (the **+ Field** menu inserts them). The file reloads automatically when you save it. Try `examples/parts.xlsx`.
- Placeholders: `{n}` / `{n:03}` (start + step per label), `{seq}`, `{date}`, `{time}`, `{Column}`.
- Cutting: auto cut; half cut and cut every N (E550W only, greyed out on the P710BT); chain printing; never cut (special tape).
- Output: high resolution 180×360, mirror, margins 2–127 mm.
- Save and open designs as `.ptlabel` JSON. ⌘P print, ⌘S save, ⌘O open, ⌘N new.
- File › Export PNG… (⇧⌘E) saves the label as previewed (current values, previewed row) as a pixel-exact PNG, the same image as `ptlabel render`.
- Undo/redo of label edits (⌘Z, ⇧⌘Z, Edit menu), up to 100 steps; a typing burst or slider drag is one step. In a text field ⌘Z undoes its own typing. The window title shows the file name and "Edited" until it is saved.
- File › Open Recent: the last 10 opened or saved designs (missing files are dropped), with Clear Menu.

## CLI

```sh
ptlabel list
ptlabel status 192.168.1.160
ptlabel render --tape 24 --label examples/asset-tag.ptlabel --out tag.png
ptlabel render --tape 6 --rotate 90 'A-12' --out bin.png
ptlabel print 192.168.1.160 --copies 3 --half-cut 'Hello\nworld'
ptlabel print /dev/cu.PT-P710BT6431 --label examples/asset-tag.ptlabel
```
