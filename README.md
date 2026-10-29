# PlicoPDF

A lightweight desktop PDF editor written in pure Rust. It is quick to start and stays responsive on older PCs.

## Features

- **Forms**: fill in text fields, checkboxes, radio buttons and drop-down lists.
- **Signatures**: draw one with the mouse, or extract the ink stroke from a photo or scan of your handwritten signature. The paper background is removed automatically.
- **Stamps**: text stamps with per-line formatting (size, bold, italic, underline, alignment) and dynamic fields (`{data}`, `{ora}`, `{utente}`, `{file}`, `{pagina}`, `{pagine}`), or image stamps and logos.
- **Free text** anywhere on the page.
- Signatures, stamps and text stay movable, resizable and deletable until you save.
- **Pages**: rotate, reorder, delete, insert another PDF, extract to a new PDF, replace with pages from another PDF.
- Multiple undo, also across saves.

The interface is currently in Italian.

## Install

Download the packages from [Releases](https://github.com/eghignatti/plicopdf/releases).

- **Windows**: `PlicoPDF-<version>-x64.msi` installs for all users, so it needs administrator rights. It adds a Start menu entry and an "Open with" entry for PDF files. For silent deployment (GPO, Intune): `msiexec /i PlicoPDF-<version>-x64.msi /qn`.
- **Linux**: `flatpak install --user PlicoPDF-x86_64.flatpak`. The Flathub remote must be configured, because the runtime comes from there.

## Build

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build --release
```

The binary is `target/release/plicopdf` (`plicopdf.exe` on Windows). To open a file directly, pass it as an argument: `plicopdf file.pdf`.

On Linux, building needs the usual X11/Wayland and OpenGL development headers.

## Platforms

Windows and Linux. It uses no native C/C++ libraries: rendering is done by [hayro](https://github.com/LaurenzV/hayro), editing by [lopdf](https://github.com/J-F-Liu/lopdf), and the UI by [egui](https://github.com/emilk/egui) with OpenGL.

## Data

Saved signatures and stamps are kept in `%APPDATA%\plicopdf` on Windows, and in `$XDG_CONFIG_HOME/plicopdf` (or `~/.config/plicopdf`) on Linux. Set `PLICOPDF_CONFIG` to use a different folder, for example when testing.

## License

Copyright (C) 2026 eghignatti. PlicoPDF is free software, released under the GNU General Public License, version 3 or (at your option) any later version. See [LICENSE](LICENSE).

The binary also bundles third-party components under their own licenses: Rust crates under GPL-compatible licenses (MIT, Apache-2.0, BSD, Zlib and similar), plus fonts and data files shipped by egui and hayro (OFL-1.1, Ubuntu Font Licence, BSD-3-Clause, CC0).
