# PTLabel — plan

Native macOS (Rust) label app for **Brother PT-P710BT** (P-touch CUBE Plus, Bluetooth) and **PT-E550W** (Wi-Fi infrastructure).

## Research findings

Source: Brother *Raster Command Reference PT-E550W/P750W/P710BT v1.02* (Brother Developer Center) plus probing the real E550W at 192.168.1.160.

| | PT-P710BT | PT-E550W |
|---|---|---|
| Link | Bluetooth Classic SPP (RFCOMM ch 1) → macOS `/dev/cu.PT-P710BT*` after pairing | TCP 9100 raw (send only); mDNS `_pdl-datastream._tcp` "Brother PT-E550W" |
| Status | `ESC i S` → 32-byte reply; unsolicited status during printing (`ESC i !`) | **`ESC i S` not answered on 9100** (verified). Same 32-byte block via **SNMP v1 GET `1.3.6.1.4.1.2435.3.3.9.1.6.1.0`** (verified: `80 20 42 30 66 … 0C 01 … 01 08` = E550W, 12 mm laminated, black on white) |
| Head | 128 pins, 180 dpi, 16 bytes/raster line; high-res 180×360 | same |
| Tapes | TZe 3.5–24 mm, HSe | TZe 3.5–24 mm, HSe 2:1 + 3:1 |
| Auto cut / mirror | yes (`ESC i M` bit 6/7) | yes |
| Half cut | **no** | yes (`ESC i K` bit 2) |
| Cut every N labels | **no** (`ESC i A` unsupported) | yes 1–99 |
| Chain printing | yes (`ESC i K` bit 3 = no-chain) | yes |
| Special tape (no cut) | yes (bit 4) | yes |

Protocol facts:
- Job: `00×100` invalidate, `ESC @`; per page: `ESC i a 01`, `ESC i z` (valid flags 0x84, type, width mm, 0, raster count LE32, page 0/1, 0), `ESC i M`, `ESC i A` (E550W), `ESC i K`, `ESC i d` margin dots, `M 02` (TIFF/PackBits), `G len data` / `Z` per line, `0C` between pages, `1A` after the last page.
- Print area (pins, centred on the 128-pin head): 3.5 mm 24, 6 mm 32, 9 mm 50, 12 mm 70, 18 mm 112, 24 mm 128.
- Raster byte 0 MSB = pin 127, byte 15 LSB = pin 0. Image top → high pins.
- Margin (feed) 2–127 mm (14–900 dots @180, double at 360). Length 4.4–1000 mm. Physical minimum tape out 24.5 mm (cutter position).
- Status: [8] err1 (01 no media, 04 cutter jam, 08 weak batt, 40 HV adapter), [9] err2 (01 wrong media, 10 cover open, 20 overheat), [10] width mm, [11] type (01 lam, 03 non-lam, 11 HS2:1, 17 HS3:1, FF incompat), [18] status type, [19] phase, [22] notification, [24] tape colour, [25] text colour.
- TZe cassettes don't report remaining length, only width/type/colour; "tape detection" = auto-detect width/type/colours on insert.

## Architecture

```
src/
  main.rs              GUI by default; CLI subcommands (list/status/render/print) for scripting + testing
  proto/{media,status,raster,packbits}.rs   tape tables, status parser, job encoder, TIFF PackBits
  transport/{bt,net,snmp,discovery}.rs      serial (/dev/cu.PT-*), TCP 9100 + SNMP, mDNS + /dev scan
  printer.rs           worker thread per printer: status polling, streaming print jobs, cancel
  label/{model,render,text,codes}.rs        label document, renderer (cosmic-text + qrcode + barcoders + image)
  app.rs               egui UI
```

- Rendering: supersample 2× (360×360 dpi grayscale), downsample to 180×180 or 180×360 (high-res), threshold → 1-bit.
- Label = horizontal row of blocks: Text (multi-line, font, auto-fit/size, bold/italic, align), QR, Barcode (Code128/39/EAN-13/UPC-A), Image (threshold/dither), Spacer. Label options: frame, invert, gap, auto/fixed length.
- Placeholders: `{n}` / `{n:03}` numbering (start/step), `{date}`, `{time}`, `{line}` batch list (one label per line).
- Jobs stream page by page (copies 1…∞, graceful Stop = current label becomes last + fed/cut). BT waits for "printing completed" per page; Wi-Fi checks SNMP before each page for errors.
- Tape auto-detect: status poll (BT `ESC i S` every 3 s idle, Wi-Fi SNMP every 2 s); layout and preview follow the loaded tape width and colours.
- Capability gating per model (half cut / cut-every-N greyed on P710BT).
- `scripts/bundle.sh` → `PTLabel.app` (Info.plist with Local Network + Bonjour keys, ad-hoc signed).

## Milestones
1. Protocol core + unit tests (PackBits doc example, status parse of real E550W bytes, job bytes).
2. Transports + CLI `list`/`status` (verify E550W SNMP live).
3. Renderer + CLI `render` → PNG.
4. Printer worker + CLI `print`.
5. egui app.
6. Bundle; hardware test print (with user OK); P710BT after pairing.
