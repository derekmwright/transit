# transit app icon

The wordmark's first lowercase t (outline unchanged) in charcoal `#1B1C1E` on the app's accent blue
`#4CB1F0`, on a rounded square with transparent corners (960 px tile on a 1024 canvas). A pale-t-on-charcoal
variant for dark surfaces lives in `assets/brand/`.

| File | What it is |
|---|---|
| `transit.svg` | Canonical vector master |
| `transit-1024.png` | 1024 px raster |
| `transit-macos-512.png` | macOS runtime window/Dock icon (Apple grid: 824/1024 body, transparent margin) |
| `transit.icns` | macOS bundle icon, PNG entries 16 to 1024 px, on the Apple grid |
| `transit.ico` | Windows icon: 16, 20, 24, 32, 40, 48, 64, 96, 128, 256 px; embedded by `apps/transit/build.rs` and used by the MSI |
| `hicolor/<size>x<size>/apps/transit.png` | Linux/FreeBSD icon theme: 16, 24, 32, 48, 64, 128, 256, 512 px, plus `scalable/apps/transit.svg` |
Regenerate with `packaging/icons.sh` (needs `resvg`, and `iconutil` on macOS for the `.icns`). The committed
rasters, `.ico` and `.icns` came from the owner's export of the same SVG; the `.icns` and the macOS PNG were
assembled with Pillow (Lanczos) from the 1024 PNG because `iconutil` is macOS-only.

License: part of the project under MIT OR Apache-2.0. The artwork is by its owner, @derekmwright.
