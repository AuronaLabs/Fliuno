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
SANS = 'system-ui, Segoe UI, PingFang SC, Microsoft YaHei, Noto Sans CJK SC, sans-serif'
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
        f'      <circle cx="2" cy="2" r="1" fill="{colors["grid"]}" opacity=".25"/>',
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
    x, y, width = (42, 208, 536) if compact else (554, 52, 512)
    rows = [
        ('提交任务', 'task/start', colors['accent']),
        ('等待审批', 'waiting_approval', colors['amber']),
        ('保存交付物', 'completed', colors['accent']),
    ]
    content = [
        f'<g transform="translate({x} {y})">',
        f'  <rect width="{width}" height="208" rx="18" fill="{colors["panel"]}" stroke="{colors["line"]}"/>',
        '  ' + text(24, 30, '一次任务', 16, colors['muted']),
        f'  <path d="M24 47H{width-24}M30 76V174" fill="none" stroke="{colors["panel_line"]}" stroke-width="1.5"/>',
    ]
    for row_y, (label, state, color) in zip((76, 125, 174), rows):
        content += [
            f'  <circle cx="30" cy="{row_y}" r="6" fill="{colors["panel"]}" stroke="{color}" stroke-width="2"/>',
            '  ' + text(53, row_y+7, label, 25 if compact else 22, colors['ink']),
            '  ' + text(301, row_y+6, state, 18 if compact else 16, color, weight=500, mono=True),
        ]
    return content + ['</g>']


def project(colors, compact):
    width, height, grid_x = (620, 454, 330) if compact else (1120, 312, 486)
    x, y = (42, 30) if compact else (54, 46)
    track = 'M251 454h44l156-156h169' if compact else 'M438 312h48l152-152h482'
    content = [
        f'<path d="{track}" fill="none" stroke="{colors["line"]}" opacity=".55"/>',
        *logo(x, y, colors),
        text(x+50, y+23, 'LOCAL  /  RUST', 15, colors['accent'], spacing=1.5, mono=True),
        text(x-4, 143 if compact else 175, 'Fliuno', 84 if compact else 92, colors['ink'], weight=740, spacing=-4),
        text(x, 181 if compact else 222, '面向工作台的本机 Agent 内核', 27 if compact else 29, colors['muted'], weight=550),
        *timeline(colors, compact),
    ]
    return cover(width, height, grid_x, 'Fliuno',
                 '面向工作台的本机 Rust Agent 内核。任务经过提交、审批和交付三个阶段。',
                 colors, content)


def guide(colors, compact):
    width, height, grid_x = (620, 202, 324) if compact else (1120, 162, 455)
    x, y = (42, 28) if compact else (54, 29)
    row_y = 147 if compact else 80
    positions = (42, 226, 410) if compact else (556, 724, 892)
    track = 'M47 147H565' if compact else 'M523 48V80H1066'
    topics = [('离线体验', colors['accent']), ('客户端接入', colors['amber']), ('内核开发', colors['muted'])]
    content = [
        *logo(x, y, colors),
        text(x+50, y+23, 'FLIUNO', 16, colors['accent'], spacing=2, mono=True),
        text(x-2, 111 if compact else 116, '文档中心', 46 if compact else 42, colors['ink'], weight=700),
        f'<path d="{track}" fill="none" stroke="{colors["line"]}" stroke-width="1.5"/>',
    ]
    for column_x, (label, color) in zip(positions, topics):
        content += [
            f'<circle cx="{column_x+5}" cy="{row_y}" r="5" fill="{color}"/>',
            f'<rect x="{column_x+19}" y="{row_y-21}" width="142" height="42" rx="9" fill="{colors["panel"]}" stroke="{colors["panel_line"]}"/>',
            text(column_x+34, row_y+8, label, 23 if compact else 21, colors['ink'], weight=600),
        ]
    return cover(width, height, grid_x, 'Fliuno 文档中心',
                 '离线体验、客户端接入和内核开发。', colors, content)


if __name__ == '__main__':
    for theme, colors in PALETTES.items():
        for form, compact in [('', False), ('-compact', True)]:
            for name, draw in [('cover', project), ('docs', guide)]:
                (OUT / f'fliuno-{name}{form}{theme}.svg').write_text(draw(colors, compact), encoding='utf-8', newline='\n')
