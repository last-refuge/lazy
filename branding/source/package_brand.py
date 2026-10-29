"""Refresh the selected-kit asset inventory and ZIP, excluding exploration history."""
from pathlib import Path
from zipfile import ZipFile, ZIP_DEFLATED
import hashlib
import json
import xml.etree.ElementTree as ET

ROOT=Path(__file__).resolve().parents[1]
EXCLUDED={'explorations-2026-10-29','.DS_Store','__pycache__'}

def current_files():
    return [p for p in sorted(ROOT.rglob('*')) if p.is_file() and not any(part in EXCLUDED for part in p.relative_to(ROOT).parts) and p.suffix!='.zip' and p.name!='asset-manifest.json']

def inventory():
    from PIL import Image
    files=[]
    for p in current_files():
        entry={'path':p.relative_to(ROOT).as_posix(),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()}
        if p.suffix.lower() in ['.png','.webp','.ico','.icns']:
            with Image.open(p) as im:
                entry.update(width=im.width,height=im.height,mode=im.mode)
                if im.mode=='RGBA':entry['hasTransparency']=im.getchannel('A').getextrema()[0]<255
        elif p.suffix=='.svg':
            doc=ET.parse(p).getroot();entry['viewBox']=doc.get('viewBox')
        files.append(entry)
    data={'product':'Lazy','direction':'Soft Signal','version':'1.0.0','approvedOn':'2026-10-29','scope':'Selected identity only; historical explorations and ZIPs excluded','fileCount':len(files),'files':files}
    (ROOT/'asset-manifest.json').write_text(json.dumps(data,indent=2)+'\n')
    return files

if __name__=='__main__':
    files=inventory()
    dest=ROOT/'lazy-soft-signal-brand-kit.zip'
    with ZipFile(dest,'w',ZIP_DEFLATED) as z:
        for p in current_files()+[ROOT/'asset-manifest.json']:
            z.write(p,Path('lazy-soft-signal')/p.relative_to(ROOT))
    with ZipFile(dest) as z:
        assert z.testzip() is None
        assert not any('explorations-' in f for f in z.namelist())
    print(f'Manifest: {len(files)} selected-kit files. ZIP verified: {dest.stat().st_size/1024/1024:.1f} MB.')
