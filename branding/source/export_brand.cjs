/* Export and inspect the approved local brand kit. No network access is used.
 * Optional env: LAZY_BRAND_NODE_MODULES, LAZY_BRAND_CHROMIUM, LAZY_BRAND_PYTHON.
 */
const fs = require('fs');
const path = require('path');
const { createRequire } = require('module');
const { pathToFileURL } = require('url');
const { execFileSync } = require('child_process');
const base = path.resolve(__dirname, '..');
const modules = process.env.LAZY_BRAND_NODE_MODULES || '/home/jason/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/node_modules';
const load = createRequire(path.join(modules, '__lazy_loader.cjs'));
const sharp = load('sharp');
const { chromium } = load('playwright');

async function ready(page) {
  await page.evaluate(() => document.fonts.ready);
  await page.locator('img').evaluateAll(imgs => Promise.all(imgs.map(i => i.complete ? Promise.resolve() : new Promise(r => { i.onload=r; i.onerror=r; }))));
  const broken=await page.locator('img').evaluateAll(imgs=>imgs.filter(i=>!i.naturalWidth).map(i=>i.src));
  if(broken.length)throw Error('Broken images: '+broken.join(', '));
}

async function raster(relative, output, width) {
  await sharp(path.join(base, relative), { density: 192 }).resize({ width }).png().toFile(path.join(base, output));
}

async function vectors() {
  for(const family of ['horizontal','stacked','wordmark','symbol']) {
    for(const color of ['forest','paper','sage','black','white']) {
      const stem=family==='horizontal'?'lazy-logo':family==='stacked'?'lazy-stacked':family==='wordmark'?'lazy-wordmark':'lazy-symbol';
      const prefix=`logos/${family}/${stem}-${color}`;
      const widths=family==='horizontal'?[600,1200]:family==='symbol'?[512]:[600];
      for(const width of widths) await raster(prefix+'.svg',prefix+`-${width}.png`,width);
    }
  }
  const sizes=[16,24,32,48,64,128,256,512,1024];
  for(const size of sizes) await raster(size<24?'icons/lazy-icon-forest-small.svg':'icons/lazy-icon-forest-rounded.svg',`icons/lazy-app-${size}.png`,size);
  for(const size of [16,32,48])await raster(size<24?'icons/lazy-icon-forest-small.svg':'icons/lazy-icon-forest-rounded.svg',`icons/favicon-${size}.png`,size);
  await raster('icons/lazy-icon-forest-square.svg','icons/apple-touch-icon.png',180);
  for(const size of [192,512])await raster('icons/lazy-icon-forest-square.svg',`icons/android-chrome-${size}.png`,size);
  await raster('icons/lazy-icon-forest-maskable.svg','icons/maskable-512.png',512);
  await raster('icons/lazy-icon-forest-square.svg','social/lazy-profile.png',400);
  const python=process.env.LAZY_BRAND_PYTHON || '/home/jason/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3';
  execFileSync(python,['-c',`
from pathlib import Path
from PIL import Image
p=Path(${JSON.stringify(base)})/'icons'
im=Image.open(p/'lazy-app-1024.png')
im.save(p/'lazy-app.ico',sizes=[(s,s) for s in [16,24,32,48,64,128,256]],append_images=[Image.open(p/'lazy-app-16.png')])
im.save(p/'lazy-app.icns')
Image.open(p/'lazy-app-256.png').save(p/'favicon.ico',sizes=[(s,s) for s in [16,32,48]],append_images=[Image.open(p/'favicon-16.png')])
`],{stdio:'inherit'});
  fs.writeFileSync(path.join(base,'icons/site.webmanifest'),JSON.stringify({name:'Lazy',short_name:'Lazy',icons:[{src:'android-chrome-192.png',sizes:'192x192',type:'image/png',purpose:'any'},{src:'android-chrome-512.png',sizes:'512x512',type:'image/png',purpose:'any'},{src:'maskable-512.png',sizes:'512x512',type:'image/png',purpose:'maskable'}],theme_color:'#183F35',background_color:'#F5F1E7'},null,2)+'\n');
}

(async()=>{
  await vectors();
  console.log('Exported logo PNGs, platform icons and manifest.');
  const browser=await chromium.launch({headless:true,executablePath:process.env.LAZY_BRAND_CHROMIUM || '/usr/bin/chromium'});
  try {
    const page=await browser.newPage({viewport:{width:1200,height:630},deviceScaleFactor:1});
    const errors=[];
    page.on('pageerror',e=>errors.push(String(e)));
    await page.goto(pathToFileURL(path.join(base,'source/social-templates.html')).href);
    await ready(page);
    for(const [id,file] of [['og-light','lazy-open-graph-light.png'],['og-dark','lazy-open-graph-dark.png'],['square','lazy-square.png']]){
      await page.locator('[data-export]').evaluateAll((nodes,id)=>nodes.forEach(n=>n.hidden=n.id!==id),id);
      await page.locator('#'+id).screenshot({path:path.join(base,'social',file)});
    }
    console.log('Rendered 3 social compositions.');
    await page.setViewportSize({width:1123,height:794});
    await page.goto(pathToFileURL(path.join(base,'previews/print-guide.html')).href);
    await ready(page);
    if(await page.locator('.sheet').count()!==8)throw Error('Expected eight manual pages');
    await page.pdf({path:path.join(base,'brand-guidelines.pdf'),preferCSSPageSize:true,printBackground:true,tagged:true,outline:true});
    console.log('Rendered the 8-page visual brand manual.');
    await page.setViewportSize({width:1360,height:1000});
    await page.goto(pathToFileURL(path.join(base,'index.html')).href);
    await ready(page);
    const fonts=await page.evaluate(()=>[...document.fonts].filter(f=>f.status!=='loaded').map(f=>f.family));
    if(fonts.length)throw Error('Unloaded fonts: '+fonts.join(', '));
    const missing=await page.locator('a[href],link[href],img[src]').evaluateAll(nodes=>nodes.map(n=>n.getAttribute('href')||n.getAttribute('src')).filter(p=>p&&!p.startsWith('#')&&!p.includes('://')));
    const absent=[...new Set(missing)].filter(p=>!['asset-manifest.json','lazy-soft-signal-brand-kit.zip','explorations-2026-10-29/index.html'].includes(p)&&!fs.existsSync(path.join(base,p.split('#')[0])));
    if(absent.length)throw Error('Missing linked files: '+absent.join(', '));
    for(const theme of ['light','dark']){
      await page.selectOption('#theme',theme);
      if(await page.getAttribute('html','data-lazy-theme')!==theme)throw Error('Theme switch failed');
      await page.screenshot({path:path.join(base,'previews',`brand-library-${theme}.png`),fullPage:true});
      await page.locator('#components').screenshot({path:path.join(base,'previews',`components-${theme}.png`)});
      for(const width of [1360,768,390,320]){
        await page.setViewportSize({width,height:1000});
        if(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth))throw Error(`Overflow at ${width}px / ${theme}`);
      }
      await page.setViewportSize({width:1360,height:1000});
    }
    await page.selectOption('#theme','system');
    await page.emulateMedia({colorScheme:'dark'});
    await page.waitForFunction(()=>document.documentElement.dataset.lazyTheme==='dark');
    await page.emulateMedia({colorScheme:'light'});
    await page.waitForFunction(()=>document.documentElement.dataset.lazyTheme==='light');
    await page.selectOption('#theme','light');
    await page.locator('#demo-primary').click();
    if(!(await page.locator('#action-status').textContent()).includes('Primary button selected'))throw Error('Reference button failed');
    await page.locator('[data-color="#183F35"]').click();
    await page.waitForFunction(()=>document.getElementById('copy-status').textContent.includes('#183F35'));
    await page.setViewportSize({width:390,height:1000});
    await page.screenshot({path:path.join(base,'previews/brand-library-mobile.png'),fullPage:true});
    await page.setViewportSize({width:1360,height:1000});
    await page.goto(pathToFileURL(path.join(base,'index.html')).href);
    await ready(page);
    await page.screenshot({path:path.join(base,'previews/brand-library-cover.png')});
    await page.locator('#logos').screenshot({path:path.join(base,'previews/logo-system.png')});
    await page.locator('#moss').screenshot({path:path.join(base,'previews/moss-pose-library.png')});
    if(errors.length)throw Error(errors.join('\n'));
    const verification={checkedOn:'2026-10-29',themes:['light','dark','system'],widths:[320,390,768,1360],localImages:'pass',localFonts:'pass',linkedAssets:'pass (manifest and ZIP checked by packaging; historical exploration link is optional outside the repository)',themeControls:'pass',referenceControls:'pass',colorInteraction:'pass',horizontalOverflow:'none',pdfPages:8,consoleErrors:[]};
    fs.writeFileSync(path.join(base,'previews/verification.json'),JSON.stringify(verification,null,2)+'\n');
    console.log('Verified local assets, fonts, light/dark/system modes, reference controls, color copying/fallback, and responsive widths 320–1360 px.');
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exit(1);});
