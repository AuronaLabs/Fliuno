#!/usr/bin/env python3
"""Generate the checked-in README and documentation covers (no dependencies)."""

from html import escape
from pathlib import Path


OUT = Path(__file__).resolve().parents[1] / 'docs' / 'assets'
PALETTES = {
    '': dict(start='#0d1b1d', end='#17302f', grid='#6e9990', line='#3a5351',
             panel='#132728', panel_line='#304a47', accent='#9ce9cd',
             logo='#112c26', ink='#f0f8f5', muted='#b7cec8', amber='#ffc697'),
    '-light': dict(start='#f7faf6', end='#e8f3ed', grid='#60877b', line='#afc9bf',
                   panel='#ffffff', panel_line='#cfdfd7', accent='#18664f',
                   logo='#ffffff', ink='#12302c', muted='#435f58', amber='#874521'),
}
SANS = 'system-ui, Segoe UI, sans-serif'
MONO = 'ui-monospace, Cascadia Mono, Consolas, monospace'


def text(x, y, label, size, color, *, weight=650, spacing=0, mono=False):
    font = MONO if mono else SANS
    return (f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" '
            f'font-weight="{weight}" letter-spacing="{spacing}" fill="{color}">{escape(label)}</text>')


def logo(x, y, colors):
    accent, ink = colors['accent'], colors['logo']
    return [
        f'<g transform="translate({x} {y})">',
        f'  <rect width="36" height="36" rx="10" fill="{accent}"/>',
        f'  <path d="M10 26V14l4-4h12M10 18h10" fill="none" stroke="{ink}" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round"/>',
        f'  <circle cx="25" cy="18" r="1.7" fill="{ink}"/>',
        '</g>',
    ]


def cover(width, height, grid_x, title, description, colors, content):
    return '\n'.join([
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">',
        f'  <title id="title">{escape(title)}</title>',
        f'  <desc id="desc">{escape(description)}</desc>',
        '  <defs>',
        '    <linearGradient id="paper" x1="0%" y1="10%" x2="100%" y2="95%">',
        f'      <stop stop-color="{colors["start"]}"/>',
        f'      <stop offset="1" stop-color="{colors["end"]}"/>',
        '    </linearGradient>',
        '    <pattern id="grid" width="20" height="20" patternUnits="userSpaceOnUse">',
        f'      <circle cx="2" cy="2" r="1" fill="{colors["grid"]}" opacity=".38"/>',
        '    </pattern>',
        f'    <clipPath id="frame"><rect width="{width}" height="{height}" rx="24"/></clipPath>',
        '  </defs>',
        '  <g clip-path="url(#frame)">',
        f'    <rect width="{width}" height="{height}" fill="url(#paper)"/>',
        f'    <rect x="{grid_x}" width="{width-grid_x}" height="{height}" fill="url(#grid)"/>',
        *(f'    {line}' for line in content),
        '  </g>',
        f'  <rect x=".5" y=".5" width="{width-1}" height="{height-1}" rx="23.5" fill="none" stroke="{colors["line"]}"/>',
        '</svg>',
        '',
    ])


def timeline(colors, compact):
    x, y, width, height = (42, 247, 535, 202) if compact else (566, 72, 497, 190)
    row_positions = (61, 115, 169) if compact else (61, 112, 163)
    state_x = 310 if compact else 282
    records = [
        ('Task submitted', 'task/start', colors['accent']),
        ('Approval required', 'waiting_approval', colors['amber']),
        ('Deliverable saved', 'completed', colors['accent']),
    ]
    timeline_end = 167 if compact else 163
    content = [
        f'<g transform="translate({x} {y})">',
        f'  <rect width="{width}" height="{height}" rx="16" fill="{colors["panel"]}" stroke="{colors["line"]}"/>',
        '  ' + text(22, 29, 'TASK TIMELINE', 15, colors['muted'], spacing=1.8, mono=True),
        f'  <path d="M22 43H{width-22}M27 61V{timeline_end}" fill="none" stroke="{colors["panel_line"]}" stroke-width="1.5"/>',
    ]
    for row_y, (label, state, color) in zip(row_positions, records):
        content += [
            f'  <circle cx="27" cy="{row_y}" r="5" fill="{color}"/>',
            '  ' + text(48, row_y+6, label, 24 if compact else 21, colors['ink']),
            '  ' + text(state_x, row_y+6, state, 17 if compact else 15, color, weight=500, mono=True),
        ]
    return content + ['</g>']


def project(colors, compact):
    width, height, grid_x = (620, 490, 300) if compact else (1120, 330, 559)
    x, y = (42, 35) if compact else (54, 42)
    track = 'M250 490h42l180-180h620' if compact else 'M509 330h42l170-170h1120'
    content = [
        f'<path d="{track}" fill="none" stroke="{colors["line"]}" opacity=".5"/>',
        *logo(x, y, colors),
        text(x+50, y+23, 'RUST  /  LOCAL EXECUTION', 15, colors['accent'], spacing=1.35, mono=True),
        text(x-3, 160 if compact else 180, 'Fliuno', 85 if compact else 92, colors['ink'], weight=740, spacing=-4.5),
        text(x, 204 if compact else 224, 'Agent kernel for workspaces', 28 if compact else 29, colors['muted'], weight=540),
        *timeline(colors, compact),
    ]
    return cover(width, height, grid_x, 'Fliuno',
                 'A local Rust agent kernel. A task is submitted, approved and saved as a deliverable.',
                 colors, content)


def guide(colors, compact):
    width, height, grid_x = (620, 216, 319) if compact else (1120, 172, 500)
    x, y = (40, 26) if compact else (54, 33)
    track = 'M42 172H564' if compact else 'M527 53H548V88H1028'
    row_y, chip_width = (172, 124) if compact else (88, 112)
    positions = (42, 229, 432) if compact else (571, 732, 903)
    topics = [('Quickstart', colors['accent']), ('Integration', colors['amber']), ('Kernel', colors['muted'])]
    content = [
        *logo(x, y, colors),
        text(x+50, y+23, 'FLIUNO DOCUMENTATION', 15, colors['accent'], spacing=1.35, mono=True),
        text(x-2, 118 if compact else 122, 'Handbook', 56, colors['ink'], weight=740, spacing=-2.5),
        f'<path d="{track}" fill="none" stroke="{colors["line"]}" stroke-width="1.5"/>',
    ]
    for column_x, (label, color) in zip(positions, topics):
        content += [
            f'<circle cx="{column_x}" cy="{row_y}" r="5" fill="{color}"/>',
            f'<rect x="{column_x+10}" y="{row_y-16}" width="{chip_width}" height="32" rx="8" fill="{colors["panel"]}" stroke="{colors["panel_line"]}"/>',
            text(column_x+21, row_y+6, label, 17 if compact else 16, colors['ink'], weight=630),
        ]
    return cover(width, height, grid_x, 'Fliuno handbook',
                 'Quickstart, integration and kernel documentation.', colors, content)


if __name__ == '__main__':
    for theme, colors in PALETTES.items():
        for form, compact in [('', False), ('-compact', True)]:
            for name, draw in [('cover', project), ('docs', guide)]:
                (OUT / f'fliuno-{name}{form}{theme}.svg').write_text(draw(colors, compact), encoding='utf-8', newline='\n')
