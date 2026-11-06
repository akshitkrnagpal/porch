"""Generate the Porch "Roof over Apps" icon set. Usage: python3 -I assets/icon/gen.py [outdir]

Requires rsvg-convert and ImageMagick. Defaults to writing next to this script.
The tray embeds the raw RGBA files (rgba/*.rgba) so no PNG decoder is needed at runtime."""
import sys, os, subprocess, json
out = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__)); os.makedirs(out, exist_ok=True)
P = json.load(open(os.path.join(os.path.dirname(__file__), 'params.json')))

def mark(state, color='#000', badge_color=None):
    r = P['roof']; d = P['dots']
    roof = f'<polyline points="{r["points"]}" fill="none" stroke="{color}" stroke-width="{r["stroke"]}" stroke-linecap="{r["cap"]}" stroke-linejoin="{r["join"]}"/>'
    extra = ''.join(f'<path d="{e}" fill="none" stroke="{color}" stroke-width="{r["stroke"]}" stroke-linecap="{r["cap"]}"/>' for e in P.get('extra_paths', []))
    op = P['stopped_dot_opacity'] if state.endswith('stopped') else 1
    dots = ''
    for x in d['xs']:
        if state.endswith('stopped') and P.get('stopped_style') == 'outline':
            dots += f'<circle cx="{x}" cy="{d["y"]}" r="{d["r"]-P["outline_stroke"]/2}" fill="none" stroke="{color}" stroke-width="{P["outline_stroke"]}"/>'
        else:
            dots += f'<circle cx="{x}" cy="{d["y"]}" r="{d["r"]}" fill="{color}" fill-opacity="{op}"/>'
    body = roof + extra + dots
    if state.startswith('peers'):
        b = P['badge']
        body = (f'<defs><mask id="m"><rect width="24" height="24" fill="#fff"/>'
                f'<circle cx="{b["cx"]}" cy="{b["cy"]}" r="{b["r"]+b["gap"]}" fill="#000"/></mask></defs>'
                f'<g mask="url(#m)">{body}</g>'
                f'<circle cx="{b["cx"]}" cy="{b["cy"]}" r="{b["r"]}" fill="{badge_color or color}"/>')
    return body

def svg(body, size=24, bg=''):
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 24 24">{bg}{body}</svg>'

def write(name, s):
    open(os.path.join(out, name), 'w').write(s)

amber = P['amber']
STATES = ['running', 'stopped', 'peers', 'peers-stopped']
for st in STATES:
    write(f'template-{st}.svg', svg(mark(st)))                       # macOS template (black + alpha)
    write(f'linux-{st}.svg', svg(mark(st, amber, P['badge_color'])))   # coloured tray
# App tile: mark inset on navy squircle-ish rounded rect
t = P['tile']
tile_bg = (f'<defs><radialGradient id="g" cx="0.5" cy="0.12" r="0.95">'
           f'<stop offset="0" stop-color="{P["tile_stops"][0]}"/><stop offset="0.55" stop-color="{P["tile_stops"][1]}"/>'
           f'<stop offset="1" stop-color="{P["tile_stops"][2]}"/></radialGradient></defs>'
           f'<rect x="-{t["pad"]}" y="-{t["pad"]}" width="{24+2*t["pad"]}" height="{24+2*t["pad"]}" rx="{t["rx"]}" fill="url(#g)"/>'
           f'<rect x="{-t["pad"]+0.15}" y="{-t["pad"]+0.15}" width="{24+2*t["pad"]-0.3}" height="{24+2*t["pad"]-0.3}" rx="{t["rx"]-0.1}" fill="none" stroke="#fff" stroke-opacity=".07" stroke-width=".3"/>')
tile = (f'<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="-{t["pad"]} -{t["pad"]} {24+2*t["pad"]} {24+2*t["pad"]}">'
        f'{tile_bg}<g transform="translate(0 {t["dy"]})">{mark("running", amber)}</g></svg>')
write('app-tile.svg', tile)

def png(src, dst, px):
    subprocess.run(['rsvg-convert', '-w', str(px), '-h', str(px), os.path.join(out, src), '-o', os.path.join(out, dst)], check=True)
os.makedirs(os.path.join(out, 'rgba'), exist_ok=True)
for st in STATES:
    for px in (16, 18, 32, 36):
        png(f'template-{st}.svg', f'template-{st}-{px}.png', px)
    png(f'linux-{st}.svg', f'linux-{st}-32.png', 32)
    # Raw 8-bit RGBA for include_bytes!: macOS template at 36px (18pt @2x), Linux at 32px.
    subprocess.run(['magick', os.path.join(out, f'template-{st}-36.png'), '-depth', '8', f'rgba:{os.path.join(out, "rgba", f"macos-{st}-36.rgba")}'], check=True)
    subprocess.run(['magick', os.path.join(out, f'linux-{st}-32.png'), '-depth', '8', f'rgba:{os.path.join(out, "rgba", f"linux-{st}-32.rgba")}'], check=True)
for px in (1024, 512, 256, 128):
    png('app-tile.svg', f'app-tile-{px}.png', px)  # 256/512/1024 feed Porch.icns

# Contact sheet: real-size rasters scaled up 8x nearest-neighbour on light & dark bars
m = lambda *a: subprocess.run(['magick', *a], check=True)
rows = []
for st in ['running', 'stopped', 'peers']:
    cells = []
    for bg, fg in (('#e9e9ea', 'black'), ('#262628', 'white')):
        for px in (16, 18):
            c = os.path.join(out, f'_c-{st}-{bg[1:]}-{px}.png')
            # recolour template to fg, keep alpha, flatten on bg, scale 8x
            m(os.path.join(out, f'template-{st}-{px}.png'), '-fill', fg, '-colorize', '100',
              '-background', bg, '-gravity', 'center', '-extent', '24x24', '-flatten',
              '-filter', 'point', '-resize', '800%', '-bordercolor', '#888', '-border', '1', c)
            cells.append(c)
    r = os.path.join(out, f'_row-{st}.png'); m(*cells, '+append', r); rows.append(r)
m(*rows, '-append', os.path.join(out, 'sheet-pixels-8x.png'))
for f in os.listdir(out):
    if f.startswith('_'): os.remove(os.path.join(out, f))
print('ok', out)
