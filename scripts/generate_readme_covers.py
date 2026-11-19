#!/usr/bin/env python3
"""Generate the checked-in README and documentation covers (no dependencies)."""

from html import escape
from pathlib import Path


OUT = Path(__file__).resolve().parents[1] / 'docs' / 'assets'
PALETTES = {
    '': dict(bg='#151417', panel='#201e25', line='#494454', grid='#35313c', ink='#fff5ec',
             muted='#c3baca', accent='#ffb789', teal='#9de2d1', tint='#34303b'),
    '-light': dict(bg='#fff7ed', panel='#fffdf8', line='#d9c9bf', grid='#eadbd0', ink='#29222b',
                   muted='#615561', accent='#983c17', teal='#17695a', tint='#f7e9de'),
}
SANS = "system-ui, 'Segoe UI', sans-serif"
MONO = "'Cascadia Mono', Consolas, monospace"


def text(x, y, label, size, fill, *, mono=False, weight=650, spacing=0, anchor='start'):
    font = MONO if mono else SANS
    return (f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" fill="{fill}" '
            f'font-weight="{weight}" letter-spacing="{spacing}" text-anchor="{anchor}">{escape(label)}</text>')


def rect(x, y, w, h, radius, fill, stroke='none'):
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{radius}" fill="{fill}" stroke="{stroke}"/>'


def path(d, color, width=1):
    return f'<path d="{d}" fill="none" stroke="{color}" stroke-width="{width}" stroke-linecap="round" stroke-linejoin="round"/>'


def logo(x, y, c):
    return (f'<g transform="translate({x} {y})">'
            + rect(0, 0, 34, 34, 9, c['accent'])
            + path('M9 25V13l4-4h12M9 17h10', c['bg'], 2.8)
            + f'<circle cx="24.5" cy="17" r="1.7" fill="{c["bg"]}"/></g>')


def cover(w, h, title, desc, c, pieces):
    return '\n'.join([
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-labelledby="title desc">',
        f'  <title id="title">{escape(title)}</title>',
        f'  <desc id="desc">{escape(desc)}</desc>',
        '  <defs>',
        f'    <clipPath id="frame">{rect(.5, .5, w-1, h-1, 23, c["bg"])}</clipPath>',
        f'    <pattern id="grid" width="18" height="18" patternUnits="userSpaceOnUse"><circle cx="2" cy="2" r="1" fill="{c["grid"]}"/></pattern>',
        '  </defs>',
        '  <g clip-path="url(#frame)">',
        '    ' + rect(0, 0, w, h, 23, c['bg']),
        *['    ' + piece for piece in pieces],
        '  </g>',
        '  ' + rect(.5, .5, w-1, h-1, 23, 'none', c['line']),
        '</svg>',
        '',
    ])


def lifecycle(x, y, w, c, compact):
    inset = 15 if compact else 22
    gap = 16 if compact else 22
    card = (w - 2 * inset - 2 * gap) // 3
    top = 47
    height = 149 if compact else 173
    footer = height - 15
    nodes = [
        ('01', 'Submit', 'M4 6h16v12H4zM4 6l8 6 8-6', c['muted']),
        ('02', 'Approve', 'M7 11V8a5 5 0 0110 0v3M5 11h14v10H5zM12 15v2', c['accent']),
        ('03', 'Deliver', 'M5 3h9l5 5v13H5zM14 3v6h5M9 14h6M9 17h6', c['teal']),
    ]
    pieces = [f'<g transform="translate({x} {y})">',
              rect(0, 0, w, height, 15, c['panel'], c['line']),
              rect(1, 1, w-2, height-2, 14, 'url(#grid)'),
              text(inset, 29, 'TASK LIFECYCLE', 11, c['muted'], mono=True, spacing=1.5)]
    for i, (number, label, icon, color) in enumerate(nodes):
        left = inset + i * (card + gap)
        pieces += [rect(left, top, card, 67 if compact else 83, 9, c['bg'], color),
                   f'<g transform="translate({left + 11} {top + 10})">{path(icon, color, 1.55)}</g>',
                   text(left + card - 10, top + 25, number, 10, color, mono=True, anchor='end'),
                   text(left + 11, top + (51 if compact else 57), label, 14 if compact else 17, c['ink'])]
        if i < 2:
            edge = left + card
            mid = top + (34 if compact else 42)
            pieces.append(path(f'M{edge+3} {mid}h{gap-6}m-4-4 4 4-4 4', c['muted']))
    pieces += [text(inset, footer, 'LOCAL RUN  /  DURABLE RECEIPTS', 10, c['muted'], mono=True, spacing=.8), '</g>']
    return pieces


def project(c, compact):
    w, h = (480, 360) if compact else (1120, 318)
    x = 24 if compact else 60
    logo_y = 23 if compact else 49
    baseline = 130 if compact else 186
    pieces = [rect(w * .45, 0, w * .55, h, 0, 'url(#grid)'),
              logo(x, logo_y, c),
              text(x + 48, logo_y + 22, 'RUST AGENT KERNEL', 12, c['accent'], mono=True, spacing=1.4),
              text(x - 4, baseline, 'Fliuno', 74 if compact else 112, c['ink'], weight=800, spacing=-4),
              text(x, baseline + (33 if compact else 43), 'From task to deliverable.', 21 if compact else 26, c['muted'], weight=500)]
    pieces += lifecycle(24 if compact else 552, 185 if compact else 72,
                        432 if compact else 508, c, compact)
    return cover(w, h, 'Fliuno — Rust agent kernel', 'Submit a task, approve an operation, and collect a deliverable.', c, pieces)


def guide(c, compact):
    w, h = (480, 220) if compact else (1120, 176)
    x = 24 if compact else 56
    pieces = [rect(w * .47, 0, w * .53, h, 0, 'url(#grid)'),
              logo(x, 24 if compact else 33, c),
              text(x + 48, 46 if compact else 55, 'FLIUNO / DOCUMENTATION', 12, c['accent'], mono=True, spacing=1.4),
              text(x - 2, 117 if compact else 127, 'Field guide', 52 if compact else 64, c['ink'], weight=750, spacing=-2)]
    start = 24 if compact else 533
    top = 144 if compact else 66
    card = 136 if compact else 166
    gap = 12 if compact else 16
    for i, (label, color) in enumerate([('Run a task', c['teal']), ('Connect', c['accent']), ('Build', c['muted'])]):
        left = start + i * (card + gap)
        pieces += [rect(left, top, card, 48, 9, c['panel'], c['line']),
                   rect(left + 11, top + 13, 24, 22, 5, c['tint']),
                   text(left + 23, top + 28, f'0{i+1}', 11, color, mono=True, anchor='middle'),
                   text(left + 43, top + 29, label, 14 if compact else 16, c['ink'])]
    return cover(w, h, 'Fliuno field guide', 'Three paths: run a task, connect a client, or build the kernel.', c, pieces)


if __name__ == '__main__':
    for theme, colors in PALETTES.items():
        for form, compact in [('', False), ('-compact', True)]:
            for name, draw in [('cover', project), ('docs', guide)]:
                (OUT / f'fliuno-{name}{form}{theme}.svg').write_text(draw(colors, compact), encoding='utf-8', newline='\n')
