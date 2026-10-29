"""Build the approved Lazy / Soft Signal vector assets, themes and web exports.

Requires Python 3.12+, Pillow and fontTools. Run from any working directory.
The approved exploration remains the immutable starting artwork.
"""
from pathlib import Path
import copy
import hashlib
import json
import re
import shutil
import sys
import xml.etree.ElementTree as ET

if Path('/tmp/lazy-brand-tools').is_dir():
    sys.path.insert(0, '/tmp/lazy-brand-tools')
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
APPROVED = ROOT / 'explorations-2026-10-29' / '01-soft-signal'
NS = 'http://www.w3.org/2000/svg'
ET.register_namespace('', NS)

PALETTE = {
    'forest': '#183F35', 'paper': '#F5F1E7', 'sage': '#A9C3A0', 'apricot': '#E9A38D',
    'porcelain': '#FFFDFA', 'fern': '#50655A', 'mist': '#CDD7C7',
    'deepForest': '#142B24', 'nightLeaf': '#1F3A31', 'paleLeaf': '#BDCDBF',
}
LIGHT = {
    'page':'#F5F1E7', 'surface':'#FFFDFA', 'surface-raised':'#FFFFFF',
    'text':'#183F35', 'text-muted':'#50655A', 'brand':'#183F35', 'on-brand':'#F5F1E7',
    'brand-hover':'#12372D', 'brand-active':'#0C2C24', 'accent':'#E9A38D', 'on-accent':'#183F35',
    'support':'#A9C3A0', 'on-support':'#183F35', 'selected':'#E2EADB', 'on-selected':'#183F35',
    'border':'#CDD7C7', 'control-border':'#708679', 'focus':'#326B58',
    'link':'#205B45', 'disabled':'#E5E8DF', 'on-disabled':'#50655A',
    'success':'#22624A', 'success-surface':'#E0EEE2', 'warning':'#7C4B14', 'warning-surface':'#F6EBCD',
    'danger':'#9D352D', 'danger-surface':'#F8E6DE', 'info':'#315D70', 'info-surface':'#E2EDF0',
}
DARK = {
    'page':'#142B24', 'surface':'#1F3A31', 'surface-raised':'#28483C',
    'text':'#F5F1E7', 'text-muted':'#BDCDBF', 'brand':'#A9C3A0', 'on-brand':'#142B24',
    'brand-hover':'#BDD4B5', 'brand-active':'#CCE0C6', 'accent':'#E9A38D', 'on-accent':'#183F35',
    'support':'#3E5E4B', 'on-support':'#F5F1E7', 'selected':'#355542', 'on-selected':'#F5F1E7',
    'border':'#49685A', 'control-border':'#87998E', 'focus':'#C7DDBD',
    'link':'#BDDFAF', 'disabled':'#314B40', 'on-disabled':'#BDCDBF',
    'success':'#B1DABC', 'success-surface':'#234336', 'warning':'#ECCA86', 'warning-surface':'#473A25',
    'danger':'#F2B4A8', 'danger-surface':'#452A29', 'info':'#B1D5DF', 'info-surface':'#253F48',
}

def svg(children, viewbox, title):
    root=ET.Element(f'{{{NS}}}svg', {'viewBox':viewbox,'role':'img','aria-label':title})
    ET.SubElement(root,f'{{{NS}}}title').text=title
    for child in children: root.append(copy.deepcopy(child))
    return root

def write_svg(path, root):
    path.parent.mkdir(parents=True,exist_ok=True)
    ET.ElementTree(root).write(path,encoding='unicode',xml_declaration=False)
    with path.open('a') as f: f.write('\n')

def recolor(root, color, suffix):
    root=copy.deepcopy(root)
    for node in root.iter():
        for attr in ('fill','stroke'):
            if node.get(attr)=='#183F35': node.set(attr,color)
        if node.get('id'): node.set('id',node.get('id')+'-'+suffix)
        for attr in ('mask','clip-path'):
            if node.get(attr): node.set(attr,re.sub(r'url\(#([^)]*)\)',lambda m:'url(#'+m[1]+'-'+suffix+')',node.get(attr)))
    return root

def logos():
    logo_source=ROOT/'source/approved-logo.svg'
    mark_source=ROOT/'source/approved-symbol.svg'
    if not logo_source.exists(): shutil.copy2(APPROVED/'logo.svg',logo_source)
    if not mark_source.exists(): shutil.copy2(APPROVED/'mark.svg',mark_source)
    original=ET.parse(logo_source).getroot()
    mark=ET.parse(mark_source).getroot()
    groups=[c for c in original if c.tag.endswith('}g')]
    variants={'forest':'#183F35','paper':'#F5F1E7','sage':'#A9C3A0','black':'#000000','white':'#FFFFFF'}
    for name,color in variants.items():
        main=recolor(original,color,'horizontal-'+name)
        write_svg(ROOT/f'logos/horizontal/lazy-logo-{name}.svg',main)
        symbol=recolor(mark,color,'symbol-'+name)
        write_svg(ROOT/f'logos/symbol/lazy-symbol-{name}.svg',symbol)
        word=ET.Element(f'{{{NS}}}g',{'transform':'translate(-104 0)'})
        word.append(groups[1])
        word_svg=svg([word],'0 0 189.9 110','Lazy wordmark')
        write_svg(ROOT/f'logos/wordmark/lazy-wordmark-{name}.svg',recolor(word_svg,color,'wordmark-'+name))
        upper=ET.Element(f'{{{NS}}}g',{'transform':'translate(72 0) scale(1.625)'})
        for child in mark:
            if not child.tag.endswith('}title'): upper.append(copy.deepcopy(child))
        lower=ET.Element(f'{{{NS}}}g',{'transform':'translate(-52 151)'})
        lower.append(copy.deepcopy(groups[1]))
        stack=svg([upper,lower],'0 0 300 260','Lazy stacked logo')
        write_svg(ROOT/f'logos/stacked/lazy-stacked-{name}.svg',recolor(stack,color,'stacked-'+name))
    # A reduced-detail face for 16–23 px. Removing subpixel pupils preserves the silhouette.
    micro=copy.deepcopy(mark)
    for mask in micro.iter(f'{{{NS}}}mask'):
        for child in list(mask):
            if child.tag.endswith('}circle'): mask.remove(child)
    write_svg(ROOT/'logos/symbol/lazy-symbol-small-forest.svg',recolor(micro,'#183F35','small'))
    for name,back,fore in [('forest','#183F35','#F5F1E7'),('sage','#A9C3A0','#142B24')]:
        for kind in ['rounded','square','maskable','small']:
            iconmark=micro if kind=='small' else mark
            scale=.61 if kind=='maskable' else .79
            offset=(96-96*scale)/2
            group=ET.Element(f'{{{NS}}}g',{'transform':f'translate({offset:.2f} {offset:.2f}) scale({scale})'})
            for child in iconmark:
                if not child.tag.endswith('}title'): group.append(copy.deepcopy(child))
            group=recolor(group,fore,'icon-'+name+'-'+kind)
            rect=ET.Element(f'{{{NS}}}rect',{'width':'96','height':'96','fill':back})
            if kind in ('rounded','small'): rect.set('rx','22')
            write_svg(ROOT/f'icons/lazy-icon-{name}-{kind}.svg',svg([rect,group],'0 0 96 96','Lazy application icon'))
    shutil.copy2(ROOT/'icons/lazy-icon-forest-small.svg',ROOT/'icons/favicon.svg')

def images():
    for path in (ROOT/'mascot').glob('*.png'):
        img=Image.open(path)
        assert img.mode=='RGBA' and img.getchannel('A').getextrema()==(0,255),path
        img.save(path.with_suffix('.webp'),'WEBP',quality=92,method=4)
        for size in (256,512):
            small=img.resize((size,size),Image.Resampling.LANCZOS)
            small.save(path.with_name(path.stem+f'-{size}.webp'),'WEBP',quality=90,method=4)
    master=ROOT/'illustrations/conversation-canopy.png'
    img=Image.open(master)
    for width in (480,960,1536):
        out=img.resize((width,round(width*img.height/img.width)),Image.Resampling.LANCZOS)
        out.save(master.with_name(f'conversation-canopy-{width}.webp'),'WEBP',quality=90,method=4)
    # Original native SVG artwork extends the existing leaf and speech-bubble motifs.
    bubbles='''<svg xmlns="http://www.w3.org/2000/svg" width="480" height="320" viewBox="0 0 480 320"><title>Conversation leaves pattern</title><g fill="#A9C3A0" opacity=".24"><path d="M30 78C30 43 95 32 124 59C145 80 129 112 92 118L71 142L66 118C44 113 30 100 30 78Z"/><path d="M250 215C250 180 315 169 344 196C365 217 349 249 312 255L291 279L286 255C264 250 250 237 250 215Z"/></g><g fill="#E9A38D" opacity=".2"><path d="M310 55Q362 15 397 73Q345 113 310 55Z"/><path d="M91 244Q143 204 178 262Q126 302 91 244Z"/></g></svg>'''
    (ROOT/'illustrations/conversation-leaves-pattern.svg').write_text(bubbles+'\n')

def fonts():
    for folder,clean in [('nunito','Nunito'),('dmsans','DMSans')]:
        dest=ROOT/'fonts'/folder
        for ext in ['ttf','woff2']:
            canonical=dest/f'{clean}-Variable.{ext}'
            if not canonical.exists():
                source=next(dest.glob('*.'+ext)); source.rename(canonical)
    css='''/* Self-hosted, unmodified font families. Keep adjacent OFL.txt files. */
@font-face{font-family:"Nunito";src:url("../fonts/nunito/Nunito-Variable.woff2") format("woff2");font-style:normal;font-weight:200 1000;font-display:swap;}
@font-face{font-family:"DM Sans";src:url("../fonts/dmsans/DMSans-Variable.woff2") format("woff2");font-style:normal;font-weight:100 1000;font-display:swap;}
'''
    (ROOT/'styles/fonts.css').write_text(css)

def lum(color):
    values=[int(color[i:i+2],16)/255 for i in (1,3,5)]
    values=[v/12.92 if v<=.04045 else ((v+.055)/1.055)**2.4 for v in values]
    return sum(a*b for a,b in zip(values,[.2126,.7152,.0722]))

def contrast(a,b):
    hi,lo=sorted([lum(a),lum(b)],reverse=True)
    return round((hi+.05)/(lo+.05),2)

def tokens():
    type_scale={
        'display':{'size':'clamp(2.75rem, 6vw, 5rem)','lineHeight':'1.05','weight':750,'tracking':'-0.035em'},
        'h1':{'size':'clamp(2.25rem, 4vw, 3.5rem)','lineHeight':'1.1','weight':750,'tracking':'-0.03em'},
        'h2':{'size':'clamp(1.75rem, 3vw, 2.5rem)','lineHeight':'1.15','weight':700,'tracking':'-0.025em'},
        'h3':{'size':'1.5rem','lineHeight':'1.25','weight':700,'tracking':'-0.015em'},
        'lead':{'size':'1.125rem','lineHeight':'1.6','weight':400,'tracking':'0'},
        'body':{'size':'1rem','lineHeight':'1.6','weight':400,'tracking':'0'},
        'small':{'size':'.875rem','lineHeight':'1.5','weight':400,'tracking':'0'},
        'caption':{'size':'.75rem','lineHeight':'1.5','weight':400,'tracking':'0'},
    }
    data={'name':'Lazy','direction':'Soft Signal','version':'1.0.0','approvedOn':'2026-10-29','palette':PALETTE,'themes':{'light':LIGHT,'dark':DARK},'typography':{'displayFamily':'Nunito','bodyFamily':'DM Sans','logoWeight':800,'scale':type_scale},'spacing':{'1':4,'2':8,'3':12,'4':16,'5':20,'6':24,'8':32,'10':40,'12':48,'16':64,'20':80,'24':96,'28':112},'radius':{'small':8,'medium':12,'large':20,'pill':999},'layout':{'maxWidth':1200,'mobileGutter':24,'desktopGutter':48,'tapTarget':44},'motion':{'fastMs':120,'standardMs':160,'enterMs':240,'easing':'cubic-bezier(.2,0,0,1)'}}
    (ROOT/'styles/tokens.json').write_text(json.dumps(data,indent=2)+'\n')
    css=['/* Lazy / Soft Signal v1.0.0. Color roles must be used in pairs. */',':root {','  color-scheme: light;']
    for name,value in LIGHT.items(): css.append(f'  --lazy-{name}: {value};')
    css+=['  --lazy-font-display: "Nunito", ui-rounded, system-ui, sans-serif;','  --lazy-font-body: "DM Sans", system-ui, sans-serif;']
    for key,value in data['spacing'].items(): css.append(f'  --lazy-space-{key}: {value/16:g}rem;')
    for key,value in data['radius'].items(): css.append(f'  --lazy-radius-{key}: {value}px;')
    for key,value in type_scale.items():
        for prop,propval in value.items(): css.append(f'  --lazy-{key}-{prop}: {propval};')
    css+=['  --lazy-content-width: 75rem;','  --lazy-gutter: clamp(1.5rem, 4vw, 3rem);','  --lazy-motion-fast: 120ms;','  --lazy-motion: 160ms;','  --lazy-motion-enter: 240ms;','  --lazy-easing: cubic-bezier(.2,0,0,1);','  --lazy-shadow: 0 8px 28px rgb(20 43 36 / .1);','}']
    props='\n'.join(f'    --lazy-{name}: {value};' for name,value in DARK.items())
    css+=['@media (prefers-color-scheme: dark) {','  :root:not([data-lazy-theme="light"]) {','    color-scheme: dark;',props,'    --lazy-shadow: 0 8px 28px rgb(0 0 0 / .22);','  }','}',':root[data-lazy-theme="dark"] {','  color-scheme: dark;',props,'  --lazy-shadow: 0 8px 28px rgb(0 0 0 / .22);','}',':root[data-lazy-theme="light"] { color-scheme: light; }','@media (prefers-reduced-motion: reduce) {', '  :root { --lazy-motion-fast: 0ms; --lazy-motion: 0ms; --lazy-motion-enter: 0ms; }','}']
    (ROOT/'styles/tokens.css').write_text('\n'.join(css)+'\n')
    results=[]
    for theme,values in [('light',LIGHT),('dark',DARK)]:
        pairs=[('body/page','text','page',4.5),('body/surface','text','surface',4.5),('body/raised','text','surface-raised',4.5),('muted/page','text-muted','page',4.5),('muted/surface','text-muted','surface',4.5),('muted/raised','text-muted','surface-raised',4.5),('button','on-brand','brand',4.5),('button/hover','on-brand','brand-hover',4.5),('button/active','on-brand','brand-active',4.5),('accent','on-accent','accent',4.5),('support','on-support','support',4.5),('selected','on-selected','selected',4.5),('link/page','link','page',4.5),('link/surface','link','surface',4.5),('focus/page','focus','page',3),('focus/surface','focus','surface',3),('control/surface','control-border','surface',3),('control/page','control-border','page',3)]
        pairs += [(s,s,s+'-surface',4.5) for s in ['success','warning','danger','info']]
        for label,fg,bg,threshold in pairs:
            ratio=contrast(values[fg],values[bg])
            assert ratio>=threshold,(theme,label,ratio)
            results.append({'theme':theme,'pair':label,'foreground':values[fg],'background':values[bg],'ratio':ratio,'minimum':threshold,'pass':True})
    (ROOT/'styles/contrast-report.json').write_text(json.dumps({'standard':'WCAG 2.x relative luminance','textMinimum':4.5,'nonTextMinimum':3,'checks':results},indent=2)+'\n')
    print(f'Contrast: {len(results)} text, control and focus pairs passed.')

def metadata():
    current=ROOT/'source/generation-manifest.json'
    if current.exists():
        original=json.loads(current.read_text())['original']
    else:
        original=json.loads((ROOT/'explorations-2026-10-29/generation-manifest.json').read_text())['concepts'][0]
    poses=json.loads((ROOT/'source/mascot-poses.json').read_text())
    generation={'tool':'built-in image_gen','approvedDirection':'Soft Signal','original':original,'additionalPoses':poses['poses'],'processing':'PNG masters copied without alteration; WebP exports resized/encoded with alpha preserved. Logos and patterns are native SVG. Social graphics are rendered HTML compositions.'}
    (ROOT/'source/generation-manifest.json').write_text(json.dumps(generation,indent=2)+'\n')

if __name__=='__main__':
    for d in ['logos/horizontal','logos/stacked','logos/wordmark','logos/symbol','icons','mascot','illustrations','fonts','styles','social','previews']:
        (ROOT/d).mkdir(parents=True,exist_ok=True)
    logos(); images(); fonts(); tokens(); metadata()
    print('Built the approved Soft Signal brand assets and style tokens.')
