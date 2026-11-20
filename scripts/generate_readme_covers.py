#!/usr/bin/env python3
"""Generate the checked-in README and documentation covers (no dependencies)."""

from html import escape
from pathlib import Path


OUT = Path(__file__).resolve().parents[1] / 'docs' / 'assets'
PALETTES = {
    '': dict(start='#111626', end='#1a2541', panel='#151e33', line='#394e74',
             ink='#f3f6ff', muted='#b1c0dd', blue='#9cbeff', gold='#ffcc91'),
    '-light': dict(start='#f7f8ff', end='#e2eaff', panel='#f9faff', line='#a7b9dc',
                   ink='#172746', muted='#465b7c', blue='#254eac', gold='#884619'),
}
SANS = 'system-ui, Segoe UI, PingFang SC, Microsoft YaHei, Noto Sans CJK SC, sans-serif'
MONO = 'ui-monospace, Cascadia Mono, Consolas, monospace'


def text(x, y, label, size, color, *, weight=600, spacing=0, mono=False):
    font = MONO if mono else SANS
    return (f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" '
            f'font-weight="{weight}" letter-spacing="{spacing}" fill="{color}">{escape(label)}</text>')


def logo(x, y, colors):
    return [
        f'<g transform="translate({x} {y})">',
        f'  <rect width="34" height="34" rx="9" fill="{colors["blue"]}"/>',
        f'  <path d="M10 25V13l4-4h11M10 17h10" fill="none" stroke="{colors["start"]}" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"/>',
        f'  <circle cx="24" cy="17" r="1.6" fill="{colors["start"]}"/>',
        '</g>',
    ]


def cover(width, height, grid_x, title, description, colors, content):
    return '\n'.join([
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">',
        f'  <title id="title">{escape(title)}</title>',
        f'  <desc id="desc">{escape(description)}</desc>',
        '  <defs>',
        '    <linearGradient id="surface" x1="0%" y1="0%" x2="100%" y2="75%">',
        f'      <stop stop-color="{colors["start"]}"/>',
        f'      <stop offset="1" stop-color="{colors["end"]}"/>',
        '    </linearGradient>',
        '    <pattern id="grid" width="22" height="22" patternUnits="userSpaceOnUse">',
        f'      <path d="M22 0H0V22" fill="none" stroke="{colors["line"]}" opacity=".48"/>',
        '    </pattern>',
        f'    <clipPath id="frame"><rect width="{width}" height="{height}" rx="24"/></clipPath>',
        '  </defs>',
        '  <g clip-path="url(#frame)">',
        f'    <rect width="{width}" height="{height}" fill="url(#surface)"/>',
        f'    <rect x="{grid_x}" width="{width-grid_x}" height="{height}" fill="url(#grid)"/>',
        *(f'    {line}' for line in content),
        '  </g>',
        f'  <rect x=".5" y=".5" width="{width-1}" height="{height-1}" rx="23.5" fill="none" stroke="{colors["line"]}"/>',
        '</svg>',
        '',
    ])


def task_flow(x, y, width, colors, compact):
    steps = [
        ('01', '提交任务', 'task/start', 'blue'),
        ('02', '审批操作', 'approval/respond', 'gold'),
        ('03', '查看交付物', 'artifacts/list', 'blue'),
    ]
    content = [
        f'<g transform="translate({x} {y})">',
        f'  <path d="M18 28V156" fill="none" stroke="{colors["line"]}" stroke-width="2"/>',
    ]
    for index, (number, label, method, tint) in enumerate(steps):
        row = index * 64
        accent = colors[tint]
        content.extend([
            f'  <rect x="49" y="{row}" width="{width - 49}" height="52" rx="10" fill="{colors["panel"]}" stroke="{colors["line"]}"/>',
            f'  <path d="M36 {row+26}H49" stroke="{colors["line"]}" stroke-width="2"/>',
            f'  <circle cx="18" cy="{row+26}" r="17" fill="{colors["panel"]}" stroke="{accent}" stroke-width="1.5"/>',
            '  ' + text(8, row+32, number, 16, accent, mono=True),
            '  ' + text(70, row+34, label, 24 if compact else 22, colors['ink']),
            '  ' + text(width - (238 if compact else 209), row+33, method,
                        21 if compact else 17, accent, weight=500, mono=True),
        ])
    return content + ['</g>']


def project(colors, compact):
    width, height, grid_x = (640, 462, 340) if compact else (1120, 334, 520)
    x, y = (38, 34) if compact else (54, 52)
    title_y, subtitle_y = (157, 207) if compact else (185, 237)
    flow_x, flow_y, flow_width = (38, 246, 564) if compact else (575, 77, 488)
    track = 'M340 0v104l104 104h196' if compact else 'M458 334h64l86-86h512M520 0v59l74 74h526'
    content = [
        f'<path d="{track}" fill="none" stroke="{colors["line"]}" stroke-width="1.5"/>',
        *logo(x, y, colors),
        text(x+49, y+23, 'LOCAL AGENT RUNTIME', 18 if compact else 16,
             colors['blue'], spacing=1.6, mono=True),
        text(x-4, title_y, 'Fliuno', 94 if compact else 100, colors['ink'], weight=770, spacing=-4),
        text(x, subtitle_y, '面向工作台的本机 Rust 内核', 30, colors['muted'], weight=550),
        *task_flow(flow_x, flow_y, flow_width, colors, compact),
    ]
    return cover(width, height, grid_x, 'Fliuno',
                 '面向工作台的本机 Rust 内核。任务经过提交、审批和交付。', colors, content)


def guide(colors, compact):
    width, height, grid_x = (640, 178, 320) if compact else (1120, 178, 495)
    x, y = (38, 28) if compact else (54, 27)
    content = [
        *logo(x, y, colors),
        text(x+48, y+23, 'FLIUNO / DOCS', 18 if compact else 16,
             colors['blue'], spacing=1.5, mono=True),
        text(x-2, 127, '开发文档', 49 if compact else 54, colors['ink'], weight=740),
    ]
    if compact:
        content += [
            f'<path d="M400 56h52l37 37h113M445 125h72l32-32" fill="none" stroke="{colors["line"]}" stroke-width="2"/>',
            f'<circle cx="489" cy="93" r="5" fill="{colors["blue"]}"/>',
            f'<circle cx="549" cy="93" r="5" fill="{colors["gold"]}"/>',
        ]
    else:
        content.append(f'<path d="M520 88H1064M647 88l-38 38h-89" fill="none" stroke="{colors["line"]}" stroke-width="2"/>')
        for column_x, (label, tint) in zip((548, 723, 898),
                                           (('离线体验', 'blue'), ('应用接入', 'gold'), ('内核开发', 'blue'))):
            content.extend([
                f'<rect x="{column_x}" y="62" width="150" height="52" rx="10" fill="{colors["panel"]}" stroke="{colors["line"]}"/>',
                f'<circle cx="{column_x+17}" cy="88" r="4" fill="{colors[tint]}"/>',
                text(column_x+31, 96, label, 22, colors['ink']),
            ])
    return cover(width, height, grid_x, 'Fliuno 开发文档',
                 '离线体验、应用接入和内核开发。', colors, content)


if __name__ == '__main__':
    for theme, colors in PALETTES.items():
        for form, compact in [('', False), ('-compact', True)]:
            for name, draw in [('cover', project), ('docs', guide)]:
                (OUT / f'fliuno-{name}{form}{theme}.svg').write_text(draw(colors, compact), encoding='utf-8', newline='\n')
