#!/usr/bin/env python3
"""Rebuild hero.png with Python 3, ImageMagick and librsvg: python3 assets/screenshots/generate-hero.py."""
import base64
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent

def data(path):
    return 'data:image/png;base64,' + base64.b64encode(path.read_bytes()).decode()

with tempfile.TemporaryDirectory() as directory:
    work = Path(directory)
    # Crop only transparent padding; preserve the actual native menu pixels.
    subprocess.run(['magick', str(ROOT / 'menu-bar.png'), '-crop', '542x704+98+79', '+repage', str(work / 'menu.png')], check=True)
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1600" height="1000" viewBox="0 0 1600 1000">
<rect width="1600" height="1000" rx="28" fill="#2b2b2e"/>
<g font-family="Arial, sans-serif" fill="#f5f5f7">
  <text x="88" y="198" font-size="64" font-weight="700">Your dev servers.</text>
  <text x="88" y="274" font-size="64" font-weight="700">Across your machines.</text>
  <text x="90" y="342" font-size="30" fill="#c7c7cc">Stable URLs, from your terminal or menu bar.</text>
  <text x="90" y="468" font-size="29" font-weight="700">Start a local app</text>
  <text x="90" y="676" font-size="29" font-weight="700">Open an app on your Mac mini</text>
  <text x="952" y="106" font-size="28" fill="#c7c7cc">The macOS menu</text>
</g>
<g fill="#202023" stroke="#48484d" stroke-width="2">
  <rect x="88" y="494" width="746" height="106" rx="12"/>
  <rect x="88" y="702" width="746" height="150" rx="12"/>
</g>
<g xml:space="preserve" font-family="Menlo, monospace" font-size="29" fill="#f5f5f7">
  <text x="118" y="557"><tspan fill="#a0a0a8">$ </tspan>porch run pnpm dev</text>
  <text x="118" y="762"><tspan fill="#a0a0a8">$ </tspan>porch open storybook \\</text>
  <text x="153" y="810">--peer mac-mini</text>
</g>
<defs><clipPath id="icon"><rect x="1414" y="75" width="84" height="49" rx="24"/></clipPath></defs>
<image clip-path="url(#icon)" x="1414" y="66" width="84" height="64" xlink:href="{data(ROOT / 'menu-bar-icon.png')}"/>
<image x="952" y="164" width="542" height="704" xlink:href="{data(work / 'menu.png')}"/>
</svg>'''
    source = work / 'hero.svg'
    source.write_text(svg)
    subprocess.run(['rsvg-convert', str(source), '-o', str(ROOT / 'hero.png')], check=True)
