"""Rebuild the static comparison gallery, vector logos and website handoff files."""
from pathlib import Path
import base64
import html
import json
import shutil
import sys

sys.path.insert(0, '/tmp/lazy-brand-tools')
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools.pens.svgPathPen import SVGPathPen
from PIL import Image

BASE = Path(__file__).resolve().parent
DATA = json.loads((BASE / 'directions.json').read_text())

MARKS = [
    '''<defs><mask id="cut"><rect width="96" height="96" fill="white"/><path d="M21 45C21 30 34 24 48 28C63 24 76 31 76 45C76 59 65 67 48 67C31 67 21 59 21 45Z" fill="black"/><ellipse cx="34" cy="45" rx="12" ry="9" transform="rotate(-24 34 45)" fill="white"/><ellipse cx="62" cy="45" rx="12" ry="9" transform="rotate(24 62 45)" fill="white"/><circle cx="35" cy="44" r="2.7" fill="black"/><circle cx="61" cy="44" r="2.7" fill="black"/></mask></defs><path d="M48 10C24 10 9 24 9 45C9 65 24 78 47 78H62L83 88L79 65C85 59 87 52 87 44C87 24 71 10 48 10Z" fill="PRIMARY" mask="url(#cut)"/>''',
    '''<defs><mask id="cut"><rect width="96" height="96" fill="white"/><path d="M72 61C79 48 73 29 59 25C43 20 27 29 25 43C22 58 32 68 44 68C57 68 64 59 61 49C59 41 49 38 43 43C38 47 40 53 45 54" fill="none" stroke="black" stroke-width="8" stroke-linecap="round"/></mask></defs><path d="M48 9C25 9 9 25 9 47C9 70 25 85 47 85C57 85 64 82 69 78L86 87L81 66C85 59 87 53 87 46C87 25 71 9 48 9Z" fill="PRIMARY" mask="url(#cut)"/>''',
    '''<defs><mask id="cut"><rect width="96" height="96" fill="white"/><circle cx="65" cy="33" r="30" fill="black"/></mask></defs><path d="M47 9C25 9 9 25 9 47C9 68 25 85 47 85C57 85 64 82 70 78L86 87L81 66C85 60 87 53 87 47C87 26 70 9 47 9Z" fill="PRIMARY" mask="url(#cut)"/><path d="M72 23L76 34L87 38L76 42L72 53L68 42L57 38L68 34Z" fill="ACCENT"/>''',
    '''<path d="M15 71V40C15 18 47 18 47 40V70M47 40C47 18 80 18 80 40V83L65 72H15" fill="none" stroke="PRIMARY" stroke-width="11" stroke-linecap="round" stroke-linejoin="round"/>''',
    '''<defs><mask id="cut"><rect width="96" height="96" fill="white"/><circle cx="39" cy="36" r="3.5" fill="black"/><circle cx="57" cy="36" r="3.5" fill="black"/></mask></defs><path d="M25 42V34C25 19 35 10 48 10C62 10 72 20 72 34V43C72 52 78 54 83 48C88 42 95 47 91 56C88 65 77 68 68 62C71 73 78 74 83 68C88 62 94 68 89 76C82 87 65 82 60 70C60 84 54 90 48 90C42 90 36 84 36 70C31 83 14 87 7 76C2 68 9 62 14 68C19 74 26 72 28 62C19 68 8 64 5 56C1 47 8 42 13 48C18 54 25 52 25 42Z" fill="PRIMARY" mask="url(#cut)"/>'''
]

def mark(i, primary, accent=None):
    return MARKS[i].replace('PRIMARY', primary).replace('ACCENT', accent or primary)

def svg(content, viewbox, title):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{viewbox}" role="img" aria-label="{html.escape(title)}"><title>{html.escape(title)}</title>{content}</svg>\n'

def font_file(folder):
    return next((BASE / 'fonts' / folder).glob('*.ttf'))

def wordmark(d):
    font = TTFont(font_file(d['displayFolder']))
    axes = {'wght': d['logoWeight']}
    if 'fvar' in font:
        names = {a.axisTag for a in font['fvar'].axes}
        if 'opsz' in names: axes['opsz'] = 72
        if 'SOFT' in names: axes['SOFT'] = 40
        if 'WONK' in names: axes['WONK'] = 1
        font = instantiateVariableFont(font, axes, inplace=False)
    glyphs = font.getGlyphSet()
    cmap = font.getBestCmap()
    units = font['head'].unitsPerEm
    scale = 92 / units
    parts, advance = [], 0
    for char in 'lazy':
        name = cmap[ord(char)]
        pen = SVGPathPen(glyphs)
        glyphs[name].draw(pen)
        parts.append(f'<path d="{pen.getCommands()}" transform="translate({advance:.1f} 0)"/>')
        advance += glyphs[name].width - units * .025
    return f'<g transform="translate(116 77) scale({scale:.6f} {-scale:.6f})">{"".join(parts)}</g>', 116 + advance * scale + 12

def luminance(color):
    rgb = [int(color[i:i+2], 16) / 255 for i in (1, 3, 5)]
    linear = [x / 12.92 if x <= .04045 else ((x + .055) / 1.055) ** 2.4 for x in rgb]
    return sum(a*b for a,b in zip(linear, [.2126, .7152, .0722]))

def contrast(a, b):
    hi, lo = sorted([luminance(a), luminance(b)], reverse=True)
    return round((hi + .05) / (lo + .05), 2)

def e(s): return html.escape(str(s))

def font_css():
    css = []
    families = {d['displayFolder']: d['display'] for d in DATA}
    families['dmsans'] = 'DM Sans'
    for folder, name in families.items():
        src = font_file(folder)
        font = TTFont(src)
        font.flavor = 'woff2'
        out = src.with_suffix('.woff2')
        font.save(out)
        css.append(f'@font-face{{font-family:"{name}";src:url("fonts/{folder}/{out.name}") format("woff2");font-weight:100 1000;font-style:normal;font-display:swap;}}')
    plex = BASE / 'fonts' / 'ibm-plex-sans'
    plex.mkdir(exist_ok=True)
    original = BASE.parents[1] / 'crates/lazy-ui/assets/fonts/ibm-plex-sans'
    for filename in ['IBMPlexSans-Regular.ttf', 'IBMPlexSans-SemiBold.ttf', 'LICENSE.txt']:
        shutil.copy2(original / filename, plex / filename)
    for filename, weight in [('IBMPlexSans-Regular.ttf',400),('IBMPlexSans-SemiBold.ttf',600)]:
        f = TTFont(plex / filename); f.flavor='woff2'; f.save((plex / filename).with_suffix('.woff2'))
        css.append(f'@font-face{{font-family:"IBM Plex Sans";src:url("fonts/ibm-plex-sans/{Path(filename).stem}.woff2") format("woff2");font-weight:{weight};font-display:swap;}}')
    (BASE / 'fonts.css').write_text('\n'.join(css) + '\n')

def build_direction(i, d):
    folder = BASE / d['slug']
    folder.mkdir(exist_ok=True)
    word, width = wordmark(d)
    for filename, foreground, accent in [
        ('logo.svg',d['primary'],d['accent']),
        ('logo-reversed.svg',d['background'] if i != 2 else d['text'],d['background'] if i != 2 else d['text']),
        ('logo-mono.svg',d['text'],d['text']),
    ]:
        logo = '<g transform="translate(0 7)">' + mark(i,foreground,accent) + '</g>' + f'<g fill="{foreground}">{word}</g>'
        (folder / filename).write_text(svg(logo,f'0 0 {width:.1f} 110','Lazy — '+d['name']))
    (folder / 'mark.svg').write_text(svg(mark(i,d['primary'],d['accent']),'0 0 96 96','Lazy '+d['name']+' symbol'))
    (folder / 'mark-mono.svg').write_text(svg(mark(i,d['text']),'0 0 96 96','Lazy '+d['name']+' monochrome symbol'))
    favicon = f'<rect width="96" height="96" rx="22" fill="{d["primary"]}"/><g transform="translate(10 10) scale(.79)">{mark(i,d["onPrimary"])}</g>'
    (folder / 'favicon.svg').write_text(svg(favicon,'0 0 96 96','Lazy app icon'))
    for asset in ['mascot', 'illustration']:
        im=Image.open(folder / f'{asset}.png')
        im.save(folder / f'{asset}.webp', 'WEBP', quality=90, method=6)
    pairs = {
        'Body on background': contrast(d['text'],d['background']),
        'Secondary text on background': contrast(d['muted'],d['background']),
        'Primary button label': contrast(d['onPrimary'],d['primary']),
        'Accent label': contrast(d['onAccent'],d['accent']),
        'Dark mode body': contrast(d['darkText'],d['darkBackground']),
        'Dark mode secondary text': contrast(d['darkMuted'],d['darkBackground']),
        'Dark mode primary button label': contrast(d['darkOnPrimary'],d['darkPrimary']),
    }
    assert min(pairs.values()) >= 4.5, (d['name'],pairs)
    d['contrast'] = pairs
    css = [f'/* Lazy / {d["name"]}. Copy alongside ../fonts.css or adjust font URLs. */', '@import url("../fonts.css");', ':root {']
    mapping = {'brand': 'primary','on-brand':'onPrimary','page':'background','surface':'surface','text':'text','text-muted':'muted','accent':'accent','on-accent':'onAccent','support':'support','border':'border','focus':'focus'}
    css += [f'  --lazy-{key}: {d[value]};' for key,value in mapping.items()]
    css += [f'  --lazy-font-display: "{d["display"]}", sans-serif;', f'  --lazy-font-body: "{d["body"]}", sans-serif;', '  --lazy-space-unit: 4px;', f'  --lazy-radius: {d["radius"]}px;', f'  --lazy-button-radius: {d["buttonRadius"]}px;', '  --lazy-content-width: 1200px;', '  --lazy-transition: 160ms ease-out;', '}', '@media (prefers-color-scheme: dark) {', '  :root {']
    darkmap={'page':'darkBackground','surface':'darkSurface','text':'darkText','text-muted':'darkMuted','brand':'darkPrimary','on-brand':'darkOnPrimary'}
    css += [f'    --lazy-{key}: {d[value]};' for key,value in darkmap.items()]
    css += ['  }','}', '@media (prefers-reduced-motion: reduce) { :root { --lazy-transition: 0ms; } }']
    (folder / 'tokens.css').write_text('\n'.join(css) + '\n')
    (folder / 'brand.json').write_text(json.dumps(d, indent=2) + '\n')

def board(i, d):
    slug=d['slug']
    colors=[d[k] for k in ['primary','background','support','accent']]
    swatches=''.join(f'<div class="swatch"><div class="chip" style="background:{color}"></div><span>{e(name)}</span><code>{color}</code></div>' for color,name in zip(colors,d['paletteNames']))
    return f'''<article class="direction direction-{i+1}" id="{slug}" style="--page:{d['background']};--surface:{d['surface']};--ink:{d['text']};--secondary:{d['muted']};--brand:{d['primary']};--on-brand:{d['onPrimary']};--accent:{d['accent']};--on-accent:{d['onAccent']};--line:{d['border']};--display:'{d['display']}';--body:'{d['body']}';--corner:{d['radius']}px;--button-corner:{d['buttonRadius']}px;">
    <div class="board-heading"><div><span class="eyebrow">DIRECTION {i+1:02d}</span><h2>{e(d['name'])}</h2></div><p>{e(d['personality'])}</p></div>
    <div class="identity-row"><div class="logo-field"><img class="wordmark" src="{slug}/logo.svg" alt="Lazy logo — {e(d['name'])}"><p>{e(d['strap'])}</p></div><div class="marks"><div class="mark-sample"><img src="{slug}/mark-mono.svg" alt="Single color mark"><span>One color</span></div><div class="mark-sample"><img src="{slug}/favicon.svg" alt="App icon"><span>App icon</span></div></div></div>
    <div class="visuals"><figure class="hero-art"><img src="{slug}/illustration.webp" alt="{e(d['illustrationIdea'])}"><figcaption>Brand illustration</figcaption></figure><figure class="mascot-art"><img src="{slug}/mascot.webp" alt="{e(d['mascotDescription'])}"><figcaption><strong>{e(d['mascot'])}</strong> / {e(d['species'])}</figcaption></figure></div>
    <div class="system-row"><div class="palette"><span class="eyebrow">COLOR SYSTEM</span><div class="swatches">{swatches}</div></div><div class="type"><span class="eyebrow">TYPE SYSTEM</span><div class="type-display">{e(d['display'])}</div><p>{e(d['body'])} / Body & interface</p><p class="type-caption">Display 600–800 · Body 400 · UI 600</p></div></div>
    <div class="site-example"><div class="site-copy"><span class="eyebrow">WEBSITE EXPRESSION</span><h3>{e(d['headline']).replace(chr(10),'<br>')}</h3><p>A native place for channels, messages and the people you work with.</p><div class="button-samples"><span class="brand-button">Get Lazy <span aria-hidden="true">↗</span></span><span class="secondary-button">Explore the app</span></div></div><div class="chat-example"><div class="chat-top"><img src="{slug}/favicon.svg" alt=""><strong># design</strong><span>3 members</span></div><div class="message"><div class="avatar">A</div><div><strong>Alex <small>10:24</small></strong><p>The new designs are ready to share.</p><div class="reaction">✦ &nbsp; 3</div></div></div><div class="message"><div class="avatar avatar-alt">J</div><div><strong>Jamie <small>10:26</small></strong><p>Lovely. I’ll take a look after lunch.</p></div></div><div class="composer">Message #design <span aria-hidden="true">↵</span></div></div></div>
    <div class="board-bottom"><p>{e(d['recommendation'])}</p><span>LAZY / BRAND EXPLORATIONS / 2026</span></div></article>'''

def detail(d):
    pairs=''.join(f'<li>{e(label)} <strong>{ratio}:1</strong></li>' for label,ratio in d['contrast'].items())
    files=' · '.join(f'<a href="{d["slug"]}/{filename}" download>{label}</a>' for filename,label in [('logo.svg','Logo SVG'),('mark.svg','Symbol SVG'),('mascot.png','Mascot PNG'),('illustration.png','Illustration PNG'),('tokens.css','CSS tokens')])
    return f'''<div class="implementation" data-detail="{d['slug']}"><div><h3>Why this direction</h3><p>{e(d['summary'])}</p><p><strong>Best for:</strong> {e(d['audience'])}</p><h3>Logo & illustration</h3><p>{e(d['logoIdea'])}</p><p>{e(d['style'])}</p><h3>Mascot role</h3><p>{e(d['mascotDescription'])}</p><p><strong>Design consideration:</strong> {e(d['tradeoff'])}</p></div><div><h3>Verified text contrast</h3><ul class="contrast-list">{pairs}</ul><p>Each listed pair clears WCAG AA’s 4.5:1 threshold for normal text. Decorative colors are not interchangeable with text tokens.</p><h3>Reusable assets</h3><p class="asset-links">{files}</p><p>Also included: reverse and monochrome wordmarks, a monochrome symbol, SVG favicon, PNG originals, WebP exports, self-hosted WOFF2 fonts, font licenses and light/dark CSS tokens.</p></div></div>'''

def build_gallery():
    nav=''.join(f'<button type="button" data-choice="{d["slug"]}" aria-pressed="false">{i+1:02d} / {e(d["name"])}</button>' for i,d in enumerate(DATA))
    entries=''.join('<section class="entry" data-entry="'+d['slug']+'">'+board(i,d)+detail(d)+'</section>' for i,d in enumerate(DATA))
    template=(BASE / 'gallery-template.html').read_text()
    result=template.replace('<!-- NAV -->',nav).replace('<!-- ENTRIES -->',entries)
    (BASE / 'index.html').write_text(result)

def build_overview():
    cards=[]
    for i,d in enumerate(DATA):
        colors=''.join(f'<span style="background:{d[k]}"></span>' for k in ['primary','background','support','accent'])
        cards.append(f'''<article style="background:{d['background']};color:{d['text']};--display:'{d['display']}';--body:'{d['body']}'"><div class="title"><span>0{i+1}</span><h2>{e(d['name'])}</h2></div><img class="logo" src="{d['slug']}/logo.svg" alt="Lazy logo"><img class="hero" src="{d['slug']}/illustration.webp" alt="Brand illustration"><div class="lower"><div><div class="colors">{colors}</div><p class="font">{e(d['display'])}</p><p class="body">{e(d['body'])}</p><p class="mascot-name">{e(d['mascot'])} / {e(d['species'])}</p></div><img class="mascot" src="{d['slug']}/mascot.webp" alt="{e(d['mascot'])}"></div></article>''')
    style='''*{box-sizing:border-box}body{margin:0;background:#e9e8e2;color:#263b30;padding:38px;font-family:"DM Sans",sans-serif}header{display:flex;justify-content:space-between;align-items:end;padding:0 0 26px}h1{font-size:42px;font-weight:600;letter-spacing:-.045em;margin:0}header p{font-size:13px;margin:0;line-height:1.7;text-align:right}main{display:grid;grid-template-columns:repeat(3,1fr);gap:22px;align-items:start}article{padding:24px;border-radius:8px;font-family:var(--body),sans-serif}.title{display:flex;align-items:center;gap:12px}.title>span{font-size:11px;letter-spacing:.1em}h2{font-family:var(--display),sans-serif;font-size:23px;font-weight:600;letter-spacing:-.03em;margin:0}.logo{width:175px;height:64px;object-fit:contain;object-position:left;margin:17px 0 13px}.hero{display:block;width:100%;aspect-ratio:1.5;object-fit:cover;border-radius:8px}.lower{display:grid;grid-template-columns:1fr 118px;gap:12px;align-items:center;padding-top:16px}.colors{display:flex;gap:5px}.colors span{height:22px;flex:1;border-radius:4px;box-shadow:inset 0 0 0 1px #80808044}.font{font-family:var(--display),sans-serif;font-size:22px;font-weight:650;line-height:1.1;margin:12px 0 4px;letter-spacing:-.035em}.body{font-size:12px;margin:0}.mascot-name{font-size:10px;margin:10px 0 0;line-height:1.5}.mascot{width:118px;height:130px;object-fit:contain}footer{grid-column:3;grid-row:2;align-self:stretch;display:flex;flex-direction:column;justify-content:center;padding:25px;color:#3f584b}footer h2{font-family:"Nunito",sans-serif;font-size:40px;line-height:1.1;letter-spacing:-.045em;margin:0 0 20px}footer p{font-size:15px;line-height:1.8;margin:0}'''
    out=f'''<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Lazy brand directions overview</title><link rel="stylesheet" href="fonts.css"><style>{style}</style></head><body><header><h1>Five ways to feel Lazy.</h1><p>Original logos · illustrations · mascots · website systems<br>LAZY / BRAND EXPLORATIONS / OCTOBER 2026</p></header><main>{''.join(cards)}<footer><h2>A calmer place<br>to work together.</h2><p>Each direction includes vector logos, a transparent mascot, a landscape illustration, font files, color tokens and a website style example.</p></footer></main></body></html>'''
    (BASE / 'overview.html').write_text(out)

def write_guide():
    lines=['# Lazy — five brand directions', '', 'Created October 29, 2026. These are complete exploration kits for choosing an identity for Lazy, the native workplace chat app. Direction names are internal concept labels; the product name remains **Lazy**.', '', 'Open `index.html` for the comparison gallery. It works offline and includes one full brand board and implementation notes per direction. Use the direction buttons to focus on one option. The gallery is a local design artifact, not a published website.', '', '## Recommendation', '', '**Soft Signal** is the strongest overall fit for the name and a calmer approach to team chat. **Good Loop** has the strongest compact graphic mark. **Afterglow** is the strongest premium, dark-first direction.', '']
    for d in DATA:
        lines += [f'## {d["name"]}', '', d['summary'], '', f'- **Mascot:** {d["mascot"]}, {d["species"].lower()}.', f'- **Logo:** {d["logoIdea"]}', f'- **Illustration:** {d["illustrationIdea"]}', f'- **Display font:** {d["display"]}, weight 600–800.', f'- **Body/UI font:** {d["body"]}, weight 400 for copy and 600 for controls.', f'- **Main colors:** brand {d["primary"]}; background {d["background"]}; support {d["support"]}; accent {d["accent"]}.', f'- **Character:** {d["personality"]}.', f'- **Best for:** {d["audience"]}', f'- **Art direction:** {d["style"]}', f'- **Consideration:** {d["tradeoff"]}', '', f'Files: `{d["slug"]}/`. Main board: `brand-board.png`.', '']
    lines += ['## Shared website specification', '', '- **Layout:** 1200 px maximum content width; 24 px mobile gutters and 48–64 px desktop gutters. Use a 4 px spacing unit; section spacing 64 px mobile and 112 px desktop.', '- **Type scale:** desktop display 64–80 px / 1.05, mobile display 40–48 px / 1.1; h2 36–48 px / 1.15; h3 24–28 px / 1.25; body 16–18 px / 1.55; controls 14–16 px / 1.2; captions at least 12 px / 1.4. Use display fonts for brand headlines, body fonts for navigation, product copy and chat.', '- **Buttons:** 44 px minimum height; 16–24 px horizontal padding; 600 weight. Use brand/on-brand as a pair. Use the supplied per-direction corner radius. Focus treatment: 3 px visible outline with a 3 px offset.', '- **Borders:** one pixel. Use text color for essential controls when a decorative border token is too subtle. Avoid shadows except for layered dialogs and small product surfaces.', '- **Motion:** 160 ms ease-out for state changes; respect reduced motion. Keep mascot motion occasional and avoid looping in core work areas.', '- **Icons:** one consistent rounded outline icon family, 20 or 24 px, 1.75–2 px stroke. Logo files are original editable SVG paths; wordmarks are outlined and do not depend on installed fonts.', '- **Logo usage:** primary horizontal lockup in navigation; symbol for app icons and tight spaces. Minimum 96 px horizontal lockup width, 24 px standalone symbol, 16 px favicon. Keep clear space equal to one quarter of the symbol width. Use the monochrome/reverse variant when needed; do not stretch, rotate or add effects.', '- **Illustration:** use the landscape illustration in marketing sections; use the separate transparent mascot in welcome, empty and success states. Keep actual text outside raster artwork. Do not replace a product screenshot with a fictional illustration. The chat examples in the gallery are styling samples, not screenshots of the app.', '- **Voice:** direct, warm and useful. Say “Message your team”, “You’re all caught up” and “Try again”. Keep wordplay in marketing headlines and remove it from errors and security-related copy.', '- **Color semantics:** brand colors do not automatically indicate success, warning or error. Pair status with a label/icon and independently verify those future status colors. Use the tested text pairs recorded in each `brand.json`.', '- **Themes:** per-direction `tokens.css` includes dark-system overrides. Afterglow starts in its dark palette; a light version would be a subsequent extension. Keep the foreground and background token pairs together.', '', '## Assets and implementation', '', 'Each direction includes `logo.svg`, `logo-reversed.svg`, `logo-mono.svg`, `mark.svg`, `mark-mono.svg`, `favicon.svg`, `mascot.png`, `mascot.webp`, `illustration.png`, `illustration.webp`, `tokens.css`, `brand.json` and `brand-board.png`.', '', 'Import the chosen `tokens.css` and preserve the relative `fonts.css` and `fonts/` paths, or update their URLs when moving assets into a website. Use SVG for marks and wordmarks, WebP for website illustrations, and the transparent PNG originals for further art work. The favicon can also serve as the starting artwork for platform-specific application icon exports.', '', 'Raster artwork was generated with the built-in **image_gen** tool. `generation-manifest.json` contains the exact mascot and illustration prompt set, reference paths and source-image paths. Mascots have true RGBA transparency. SVG logos were drawn as vector geometry and font outlines. Generated artwork is exploratory.', '', '## Font sources and licenses', '', 'Font binaries are included for offline review and self-hosting. Each family includes its upstream SIL Open Font License. Keep the corresponding license with redistributed fonts.', '']
    for folder,name in [('nunito','Nunito'),('dmsans','DM Sans'),('spacegrotesk','Space Grotesk'),('sora','Sora'),('fraunces','Fraunces'),('bricolagegrotesque','Bricolage Grotesque')]:
        lines += [f'- [{name} — Google Fonts upstream](https://github.com/google/fonts/tree/main/ofl/{folder}); license at `fonts/{folder}/OFL.txt`.']
    lines += ['- [IBM Plex — IBM upstream](https://github.com/IBM/plex); existing project font source, license at `fonts/ibm-plex-sans/LICENSE.txt`.', '', '## Rebuilding', '', 'Run `prepare-fonts.py` only to refresh upstream font sources and install temporary font tooling; it requires network access and Python with pip. Run `build.py` with Pillow and fontTools available to rebuild the gallery, vector wordmarks, WebP assets and tokens. `render-gallery.cjs` uses Playwright to render the five PNG boards and verify the review controls at desktop and mobile widths.', '']
    (BASE / 'README.md').write_text('\n'.join(lines))

if __name__ == '__main__':
    font_css()
    for i,d in enumerate(DATA): build_direction(i,d)
    build_gallery()
    build_overview()
    write_guide()
    (BASE / 'contrast-report.json').write_text(json.dumps({d['name']:d['contrast'] for d in DATA},indent=2)+'\n')
    print('Built 5 brand kits; all 35 text-pair contrast checks pass 4.5:1.')
