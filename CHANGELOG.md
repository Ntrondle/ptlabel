# Changelog

All notable changes to PTLabel are documented in this file.

## [2.4.0] - 2026-11-25

### Chain Printing & Cutting
- **Feed & Cut button for Chain printing:** When **Chain printing** is enabled and a finished label is sitting inside the printer waiting for the next job, a square scissors **Cut** button appears directly beside **Print**. Clicking it feeds out and cuts the waiting label cleanly without ejecting an extra white blank label, then hides automatically until the next chained print.

### Workspace & UI
- **Resizable live preview:** Drag the grip handle below the preview box to resize it (persisted across sessions), or double-click to toggle compact mode. Shrinking the preview dynamically expands the Excel table so large spreadsheets show more rows at once.
- **Dedicated scroll pill rail:** Switched scrollbars to a dedicated non-overlapping right-hand track across the editor and Excel table so scroll pills never cover buttons, card borders, or cells.
- **Cleaner toolbar & Print panel footer:** Moved the `Sequence:` / `Time:` / `Columns:` placeholder reminder into the empty space at the bottom of the Print panel (right above the `⌘P / ⌘S / ⌘O / ⌘N` shortcuts) so the editor extends all the way to the bottom edge; centered and balanced the `LENGTH` / `ADD` section labels at matching `32 px` height; flattened `align_buttons` layout and matched button heights across toolbar and text rows.

## [2.3.0] - 2026-11-25

### Spreadsheet & Excel Selection
- **Merged cells support:** Automatically detects and fills Excel merged regions (`Fill merged`) so every replicate row inherits its group values (e.g. cell line, condition) when previewing and printing.
- **Smart header detection:** Auto-detects the real header row below sparse title/note banners, trims leading empty columns, and allows overriding via right-click (`Use Row N as header row`).
- **Multi-column insertion (`{col1} {col2}`):** Click multiple column headers directly in the table (`1 · Col`, `2 · Col`, …) and click **Add to label** right after **All / Invert / None** to insert them together. The block-level **+ Field** menu also stays open while inserting multiple fields.
- **Faster row selection:** Click-and-drag painting across rows (gap-free), `⇧`-click ranges, tri-state header select-all checkbox, **Invert** button, right-click group selection (`Select all / Only select / Deselect` matching cell values), full single-line display of multi-line cells with hover tooltips, and a **Preview** row stepper (`‹ Row N ›`).
- **Drag and drop:** Drop `.xlsx`, `.xls`, `.ods`, `.csv`, `.ptlabel`, or image files directly onto the window.

### Layout, Rendering & UI
- **Fixed-length auto-scaling:** Auto-sized Text and Sequence blocks now scale down proportionally to fit inside fixed-length labels instead of overflowing.
- **Unified Fixed-length & Presets control:** Merged length presets into the Fixed-length typing dropdown (auto-focused custom mm input on top, container presets below).
- **Update window changelog:** Displays formatted, scrollable **What's new** release notes for every version between the installed build and the latest GitHub release, plus a link to GitHub release notes.
- **Toolbar & status bar polish:** Distinct uppercase `LENGTH` / `ADD` section labels, segmented `Auto | Fixed` toggle, crisp 4-bar text alignment icon buttons, real system bold font for the `123` Sequence icon and changelog bold text, full-height Print panel with bottom shortcuts, and dedicated multi-line `Sequence:` / `Time:` / `Columns:` placeholder reminder in the bottom bar.

### Stability & Windows USB
- **Windows USB (`usbprint.sys`) reliability:** Heap-owned overlapped `Xfer` transfers keep Bulk-IN reads queued instead of cancelling mid-poll (fixing UI stuck at "Connecting…"), keep the USB handle across status timeouts, and add `PTLABEL_USB_TRACE=1` diagnostics.
- **macOS file dialog crash fix:** Dispatched native `rfd` file dialogs off the `App::ui` frame to prevent macOS winit re-entrancy crashes during drag events.
- **Yellow Dev build (`--features dev`):** Separate `PTLabel Dev.app` with yellow icon, isolated profile, `scripts/dev.sh` fast launcher, live-reloaded `dev-ui.json`, and in-app Dev UI Inspector (`⌘⇧D`).

## [2.2.1] - 2026-11-24
- Windows USB printer support via built-in `usbprint.sys` (SetupAPI enumeration, PID table + IEEE 1284 fallback).
- Added `usb` CLI target and PT-P710BT status model code `0x71`.

## [2.2.0] - 2026-11-24
- Native Windows build (`.exe`) with Bluetooth RFCOMM, DirectX 12 renderer, in-window File/Edit menu bar, and self-update.
- Collapsible spreadsheet and block editor boxes with one-line summaries.
- Refreshed UI layout and larger Print button.

## [2.1.2] - 2026-11-23
- Direct macOS IOBluetooth RFCOMM channel for PT-P710BT, fixed-length lab container presets, CLI docs, and `{seq}`/`{seq2}` hiding when referenced in text.
