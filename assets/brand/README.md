# Codypendent identity

Selected on 19 October 2026, **Fold** is a geometric C made from three folded
planes: a gold upper plane, an ivory stem and a dark petrol lower plane. The
selected concept is the source of its geometry. The petrol app tile is designed
for macOS Dock, Finder and the desktop sidebar. The monochrome editor mark uses
the same folded C with open seams and inherits the editor's theme.

- `codypendent-fold-concept.png`: selected 04 / Fold concept reference.
- `codypendent-fold.svg`: flat colour symbol on a transparent background.
- `codypendent-icon-master.png`: lossless generated source artwork.
- `../../extensions/vscode/media/icon.svg`: small monochrome editor mark.
- `../../apps/desktop/src-tauri/icons/`: generated desktop sizes and ICNS.
- `../../apps/desktop/public/`: lightweight sidebar and browser variants.

Regenerate shipped sizes from the repository root:

```sh
node apps/desktop/scripts/generate-icons.mjs
```

Keep the master at its original resolution. Use the supplied 128 px sidebar
asset in the interface; do not load the master into every window. The Fold app
master is generated from the selected concept with OpenAI image generation.

The final image-generation prompt extracts and enlarges the approved concept's
lower-right app tile, preserving its folded C geometry, gold and ivory planes,
petrol background and transparent outer corners, without text or extra artwork.
