# PTLabel CLI

For scripts and agents. It prints to the Brother PT-P710BT (Bluetooth) and PT-E550W (Wi-Fi) with the
same renderer and print code as the app. A preview rendered by the CLI is exactly what prints.

## Binary

The app binary is the CLI: run it with a command to use the CLI, with no arguments to open the app.

```sh
PT=/Applications/PTLabel.app/Contents/MacOS/PTLabel   # installed from the DMG
PT=~/Applications/PTLabel.app/Contents/MacOS/PTLabel  # scripts/bundle.sh --install
PT=target/release/ptlabel                              # cargo build --release
```

Bluetooth as described here needs PTLabel 2.1.2 or later: the CLI opens the printer's serial channel
itself (IOBluetooth). Older builds go through `/dev/cu.PT-…`, which stops connecting once the printer
has been off.

Exit status: 0 on success, 1 on any error. The error is printed on stderr as `Error: <message>`.

## Commands

| Command | Does |
|---|---|
| `$PT list` | Finds printers for about 4 s and prints one line each: description, then the **target**. |
| `$PT status <target>` | Reads the printer's status: state and loaded tape. |
| `$PT render [options] [text]` | Renders a label to a PNG, without a printer. Needs `--tape`. |
| `$PT print <target> [options] [text]` | Prints. The tape width comes from the printer. |
| `$PT --help` | Usage. |

**Targets**
- Bluetooth: `/dev/cu.<Bluetooth name>`, e.g. `/dev/cu.PT-P710BT9475`. Anything that starts with `/dev/` is taken as Bluetooth, and the part after `cu.` names the printer. The printer must be paired with this Mac once (System Settings › Bluetooth).
- Wi-Fi: an IP address or host name, e.g. `192.168.1.160`.

`list` prints targets exactly as `status` and `print` take them:

```
PT-P710BT · Bluetooth (PT-P710BT9475)    /dev/cu.PT-P710BT9475
PT-E550W · Wi-Fi 192.168.1.160           192.168.1.160
```

## Label content

Use either text or a label file.

**Text** (when there's no `--label`)
- The remaining words are joined with spaces. `\n` starts a new line. `--` ends the options, so text may start with `-`.
- This gives one text block, auto-fitted to the tape height.
- Example: `$PT print /dev/cu.PT-P710BT9475 'Buffer A\n{date}'`

**Label file** (`--label FILE.ptlabel`)
- A design saved by the app (File › Save) or written by hand (JSON, see below).
- Any text on the command line is then ignored.

## Options

| Option | Default | Effect |
|---|---|---|
| `--label FILE` | | Label file instead of text. |
| `--copies N` | 1 | Copies of each label. `{n}` counts up across all copies. |
| `--tape MM` | 12 | `render` only: 3.5, 6, 9, 12, 18 or 24. |
| `--out FILE` | `label.png` | `render` only: the PNG path. |
| `--rotate 90\|180\|270` | 0 | Rotates command-line text. Not used with `--label`: rotation is per block in the file. |
| `--margin MM` | 2 | Blank tape at each end, 2–127 mm. 2 mm is the printer's minimum. |
| `--no-cut` | cut | Doesn't cut between labels. |
| `--chain` | off | Doesn't feed and cut after the last label: saves about 25 mm of tape. The strip is cut when the next job starts. |
| `--half-cut` | off | PT-E550W only (ignored on the P710BT): cuts through the tape but not its backing. |
| `--cut-every N` | 1 | PT-E550W only: full cut every N labels. |
| `--hires` | off | 180×360 dpi. |
| `--mirror` | off | Mirror image, for the back of clear tape. |

Placeholders in text, QR, Data Matrix and barcode data:

| Placeholder | Becomes |
|---|---|
| `{n}` | Label number: 1, 2, 3… |
| `{n:04}` | Label number zero-padded to 4 digits: 0001… |
| `{date}` | Today, as 2026-11-24 |
| `{time}` | Now, as 14:05 |
| `{seq}` | Value of the first Sequence block (`{seq2}`, `{seq3}`, … for additional Sequence blocks). Writing a sequence's placeholder in a Text block suppresses that Sequence block's own rendered text so it only appears in the Text block. |

Unknown placeholders are printed as written. Spreadsheet columns (`{Column}`) only work in the app. From the CLI, write the values into the text, with one `print` call or one file per row.

## Output

**`status`** prints four lines:

```
raw     80 20 42 30 76 30 00 00 00 00 12 01 01 00 …   (the 32-byte status frame)
model   ? (0x76)                                       (not reliable; ignore)
state   Ready
tape    18 mm laminated · black on white (112 printable pins)
```

- `state`
  - `Ready` or `Printing` when there's no error.
  - Otherwise one or more of these, separated by commas: `No tape`, `Cover open`, `Cutter jam`, `Weak batteries`, `Wrong AC adapter`, `Wrong tape for this job`, `Overheated`, `Incompatible tape`.
- `tape`: `none` when no cassette is loaded. Otherwise the first number is the width to pass to `render --tape`.

**`render`** prints `<out> -> <w>x<h> dots, <length> mm long (…)` on stdout. Problems go to stderr as `warning: …`:
- `warning: content is longer than the fixed length`: the label came out longer than `length_mm`. Use smaller `size_pt`, a smaller `gap_mm` or less content.
- Block errors, e.g. data a barcode can't encode.

**`print`** shows progress on stderr (`printed 2/3`), then `done: 3 label(s)` on stdout. It checks the printer first and fails without printing when there's an error or no tape (`Error: no tape loaded`, `Error: Cover open`).

## Label file (`.ptlabel`, JSON)

Blocks are laid out left to right along the tape. Every field is optional; missing fields take the default.

```json
{
  "blocks": [
    {"DataMatrix": {"data": "S-{n:04}"}},
    {"Text": {"text": "S-{n:04}\n{date}", "size_pt": 5, "bold": true, "align": "Left"}}
  ],
  "gap_mm": 1.0,
  "fixed_length": true,
  "length_mm": 25.0
}
```

This one is a 25 mm label for a 1.5 ml tube on 12 mm tape.

### Label fields

| Field | Default | |
|---|---|---|
| `blocks` | one text block "Label" | List of blocks, see below. |
| `gap_mm` | 2.0 | Space between blocks. |
| `fixed_length` | false | false: the length follows the content. |
| `length_mm` | 50.0 | Total length with margins, used when `fixed_length` is true. |
| `align` | `"Center"` | Where the content sits in a fixed length: `Left`, `Center` or `Right`. |
| `frame` | false | Draws a border. |
| `invert` | false | White on black. |

### Blocks

Each block is an object with one key, its type:

| Block | Fields (default) |
|---|---|
| `{"Text": {…}}` | `text`; `font` (`""` = system sans-serif, or an installed family name); `size_pt` (`null` = auto-fit to the tape height); `bold`, `italic`, `underline` (false); `align` (`"Center"`); `line_spacing` (1.0); `rotation`: `"R0"`, `"R90"`, `"R180"` or `"R270"` (`"R0"`). With R90 or R270 the lines run across the tape, which suits narrow tape. |
| `{"Qr": {…}}` | `data`; `ec`: `"L"`, `"M"`, `"Q"` or `"H"` (`"M"`) |
| `{"DataMatrix": {…}}` | `data`; `shape`: `"Auto"`, `"Square"` or `"Rectangle"` (`"Auto"`) |
| `{"Barcode": {…}}` | `kind`: `"Code128"`, `"Code39"`, `"Ean13"`, `"Ean8"` or `"UpcA"` (`"Code128"`); `data`; `show_text` (true); `module`, the narrow bar width in dots of 0.14 mm (2) |
| `{"Sequence": {…}}` | `prefix` (`""`); `start` (1); `step` (1); `count` (10); `digits`, zero-padding (3); `suffix` (`""`); `style`, a Text block's fields for the font. The job then prints `count` labels (× `--copies`). |
| `{"Image": {…}}` | `path`, absolute; `threshold` (128); `dither` (false); `invert` (false) |
| `{"Spacer": {"mm": 3.0}}` | Blank space |

Codes and images are as tall as the tape allows. Auto-fit text fills the tape height, so on narrow tape set `size_pt` when the length is fixed. Render first to check.

### Lengths for lab containers

These are the app's presets. Set `"fixed_length": true` and `"length_mm"` to the value.

| Container | `length_mm` |
|---|---|
| Microscope slide, across the frosted end | 25 |
| 1.5 ml tube, along the side | 25 |
| 2 ml tube, along the side | 28 |
| 15 ml Falcon, along the side | 50 |
| 50 ml Falcon, along the side | 60 |

## Recommended sequence for an agent

```sh
$PT list                                                  # the target
$PT status /dev/cu.PT-P710BT9475                          # must say "state Ready"; note the tape width
$PT render --tape 12 --label tube.ptlabel --out /tmp/preview.png
                                                          # no "warning:" lines; length as intended; look at the PNG
$PT print /dev/cu.PT-P710BT9475 --label tube.ptlabel --copies 3
```

- Use the tape width that `status` reports for `render`, or the preview won't match the print.
- Printing uses tape. Confirm with the person before long runs. A Sequence block prints `count` labels.
- Run one command at a time per printer.

## Printer limits

- **Margins:** 2 mm minimum at each end.
- **Shortest cut label:** 24.5 mm of tape, because of the distance from the print head to the cutter. Shorter print data still comes out on 24.5 mm.
- **Print data:** 4.4 mm to 1000 mm long.
- **Printable height:** a strip in the middle of the tape, narrower than the tape. `status` reports it in pins of 0.14 mm, e.g. 112 pins ≈ 15.8 mm on 18 mm tape.

## Bluetooth notes (PT-P710BT)

- **One connection at a time.** The printer has a single Bluetooth serial channel. If the PTLabel app is open and connected to the same printer, quit it or switch it to another printer before using the CLI, or the CLI can't connect.
- **Printer off:** `status` or `print` gives up after about 15 s with `Error: Bluetooth: the printer didn't answer. Is it on?`.
- **Not paired, or no Bluetooth permission:** `Error: … isn't paired with this Mac, or PTLabel may not use Bluetooth`. The process that runs the CLI needs Bluetooth access, and macOS asks for its host app (Terminal, the agent's app…) the first time.
- **Don't click Connect** on the printer in the macOS Bluetooth menu. macOS then gives the printer to its braille-display service for about a minute, and connections fail until it lets go.
- **Speed:** connecting takes 1–4 s. Each command connects and disconnects.
