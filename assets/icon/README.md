# Porch icon

The tray embeds `rgba/*.rgba` (macOS 36px template, Linux 32px). `scripts/package.py` builds `Porch.icns` from `app-tile-{256,512,1024}.png`. After changing `params.json`, regenerate everything with `python3 -I assets/icon/gen.py` (needs `rsvg-convert` and ImageMagick) and commit the outputs.

## Design

- **Mark:** "Roof over Apps", a roof chevron over three dots: your apps under one roof.
- **Geometry:** 24-unit grid. Roof `2.5,12.5 12,4.5 21.5,12.5`, stroke 2.25, round caps and joins. Dots at x 5.5/12/18.5, y 18, r 2.
- **States:** running = solid dots; stopped = dots at 50% opacity; peers = badge at (20.4, 3.5) r 2.2 with a 1.9 knockout gap.
- **Colour:** monochrome grayscale only.
  - macOS menu bar: template image (black + alpha).
  - Linux tray: #f2f2f2 mark and a white badge. Light Linux panels may need a dark variant.
  - App tile: #f2f2f2 mark.
- **Tile background:** "porch light", a radial gradient centred above the roof (cx 0.5, cy 0.12, r 0.95) with stops #555555 → #262626 (55%) → #0e0e0e.
