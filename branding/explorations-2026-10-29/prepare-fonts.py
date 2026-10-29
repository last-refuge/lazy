"""Download upstream Google Fonts and their licenses; install build-only tooling in /tmp."""
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import json
import subprocess
import sys
import urllib.request

BASE = Path(__file__).resolve().parent
FAMILIES = ['nunito', 'dmsans', 'spacegrotesk', 'sora', 'fraunces', 'bricolagegrotesque']

def download(family):
    dest = BASE / 'fonts' / family
    dest.mkdir(parents=True, exist_ok=True)
    req = urllib.request.Request(
        'https://api.github.com/repos/google/fonts/contents/ofl/' + family,
        headers={'User-Agent': 'Lazy-brand-exploration'},
    )
    with urllib.request.urlopen(req, timeout=30) as response:
        files = json.load(response)
    chosen = [f for f in files if (f['name'].endswith('.ttf') and 'Italic' not in f['name']) or f['name'] == 'OFL.txt']
    for f in chosen:
        urllib.request.urlretrieve(f['download_url'], dest / f['name'])
    return family, [f['name'] for f in chosen]

if __name__ == '__main__':
    with ThreadPoolExecutor(max_workers=6) as pool:
        for result in pool.map(download, FAMILIES):
            print(result)
    subprocess.run([sys.executable, '-m', 'pip', 'install', '--target', '/tmp/lazy-brand-tools', 'fonttools', 'brotli'], check=True)
