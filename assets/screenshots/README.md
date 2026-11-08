# README demo image

`menu-bar.png` is a real macOS capture of Porch 0.5.0 with demo data:
web, api and docs on this device; storybook and admin on a paired Mac mini.
The demo's proxy settings use port 80. `menu-bar-icon.png` is the actual Porch
status item captured with that menu open. No personal desktop content is included.

`hero.png` places those captures beside illustrative command examples. The
commands are typeset documentation, not captured terminal output. They are also
available as copyable text in the main README.

To rebuild on macOS with Python 3, ImageMagick and librsvg installed:

```sh
python3 assets/screenshots/generate-hero.py
```

The generator contains the layout, text and colors. If replacing the native
menu capture, update its crop rectangle in the generator to match the new image.
The image is 1600 × 1000 and displayed at 800 pixels wide in the README.
