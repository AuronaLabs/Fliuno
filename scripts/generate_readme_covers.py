#!/usr/bin/env python3
"""Generate the checked-in README and documentation covers (no dependencies)."""

from html import escape
from pathlib import Path


OUT = Path(__file__).resolve().parents[1] / 'docs' / 'assets'
PALETTES = {
    '': dict(bg='#121b1a', panel='#1b2725', line='#3a504b', grid='#314440', ink='#f0f8f4',
             muted='#b2c6bf', accent='#a4e8c8', amber='#ffca9e'),
    '-light': dict(bg='#f1f7f3', panel='#fbfdfa', line='#b3c9be', grid='#d4e4dc', ink='#172c26',
                   muted='#466158', accent='#1b6e54', amber='#8a471c'),
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
    inset, first, spacing = (16, 56, 34) if compact else (24, 75, 42)
    height = 161 if compact else 201
    nodes = [
        ('Task submitted', 'task/start', c['muted']),
        ('Approval required', 'waiting_approval', c['amber']),
        ('Deliverable saved', 'completed', c['accent']),
    ]
    pieces = [f'<g transform="translate({x} {y})">',
              rect(0, 0, w, height, 10, c['panel'], c['line']),
              text(inset, 24 if compact else 29, 'TASK TRACE', 12, c['muted'], mono=True, spacing=1.3),
              text(w-inset, 24 if compact else 29, 'LOCAL RUN', 12, c['muted'], mono=True, anchor='end'),
              path(f'M{inset} {first-19}H{w-inset}', c['line']),
              path(f'M{inset+5} {first}v{2*spacing}', c['line'], 2)]
    for i, (label, state, color) in enumerate(nodes):
        mid = first + i * spacing
        pieces += [rect(inset, mid-5, 10, 10, 3, color),
                   text(inset+24, mid+5, label, 17 if compact else 20, c['ink']),
                   text(w-inset, mid+4, state, 13 if compact else 15, color, mono=True, anchor='end')]
    pieces.append('</g>')
    return pieces


def project(c, compact):
    w, h = (440, 326) if compact else (1120, 292)
    x = 22 if compact else 56
    logo_y = 20 if compact else 45
    pieces = [rect(w//2, 0, w-w//2, h, 0, 'url(#grid)'),
              logo(x, logo_y, c),
              text(x+48, logo_y+22, 'LOCAL AGENT KERNEL', 12 if compact else 13,
                   c['accent'], mono=True, spacing=1.4)]
    if compact:
        pieces += [text(x-2, 106, 'Fliuno', 59, c['ink'], weight=780),
                   text(x, 134, 'From task to deliverable.', 20, c['muted'], weight=500)]
    else:
        pieces += [text(x-5, 173, 'Fliuno', 104, c['ink'], weight=780),
                   text(x, 215, 'From task to deliverable.', 27, c['muted'], weight=500)]
    pieces += lifecycle(x if compact else 544, 145 if compact else 45,
                        w-2*x if compact else 520, c, compact)
    return cover(w, h, 'Fliuno | Local agent kernel',
                 'A local task trace: submit a task, review the approval, and collect the saved deliverable.',
                 c, pieces)


def guide(c, compact):
    w, h = (440, 106) if compact else (1120, 140)
    x = 22 if compact else 56
    y = 36 if compact else 53
    pieces = [rect(w//2, 0, w-w//2, h, 0, 'url(#grid)'),
              logo(x, y, c),
              text(x+48, y-(2 if compact else 8), 'FLIUNO / DOCS', 12, c['accent'], mono=True, spacing=1.4),
              text(x+48, y+36, 'Handbook', 35 if compact else 48, c['ink'], weight=750)]
    entry, end = (305, 409) if compact else (615, 929)
    center = h // 2
    split = entry + (end - entry) // 3
    step = 19 if compact else 32
    pieces += [path(f'M{entry} {center}H{end}M{split} {center}v-{step}H{end}M{split} {center}v{step}H{end}', c['line'], 1.5),
               rect(entry - 4, center - 4, 8, 8, 2, c['accent'])]
    for dy, label, color in [(-step, '01  QUICKSTART', c['accent']),
                             (0, '02  INTEGRATE', c['amber']),
                             (step, '03  KERNEL', c['muted'])]:
        pieces.append(rect(end-3, center+dy-3, 6, 6, 2, color))
        if not compact:
            pieces.append(text(end+15, center+dy+5, label, 13, color, mono=True))
    return cover(w, h, 'Fliuno handbook', 'Three reading paths: quickstart, integration, and kernel development.', c, pieces)


if __name__ == '__main__':
    for theme, colors in PALETTES.items():
        for form, compact in [('', False), ('-compact', True)]:
            for name, draw in [('cover', project), ('docs', guide)]:
                (OUT / f'fliuno-{name}{form}{theme}.svg').write_text(draw(colors, compact), encoding='utf-8', newline='\n')
