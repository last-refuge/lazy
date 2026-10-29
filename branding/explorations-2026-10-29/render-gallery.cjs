// Render final boards and check that the gallery works at desktop and mobile widths.
const { createRequire } = require('module');
const fs = require('fs');
const path = require('path');
const { pathToFileURL } = require('url');
const bundleRequire = createRequire('/home/jason/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/node_modules/');
const { chromium } = bundleRequire('playwright');
const base = __dirname;
const directions = JSON.parse(fs.readFileSync(path.join(base, 'directions.json'), 'utf8'));

(async () => {
  const browser = await chromium.launch({ headless: true, executablePath: process.env.LAZY_CHROMIUM_PATH || '/usr/bin/chromium' });
  const page = await browser.newPage({ viewport: { width: 1360, height: 1000 }, deviceScaleFactor: 1.5 });
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  await page.goto(pathToFileURL(path.join(base, 'index.html')).href);
  await page.evaluate(() => document.fonts.ready);
  await page.locator('img').evaluateAll(imgs => Promise.all(imgs.map(img => img.complete ? Promise.resolve() : new Promise(r => { img.onload = r; img.onerror = r; }))));
  const failed = await page.locator('img').evaluateAll(imgs => imgs.filter(i => !i.complete || !i.naturalWidth).map(i => i.src));
  if (failed.length) throw Error('Images did not load: ' + failed.join(', '));
  const fonts = await page.evaluate(() => [...document.fonts].filter(f => f.status !== 'loaded').map(f => f.family));
  if (fonts.length) throw Error('Fonts did not load: ' + fonts.join(', '));
  for (const d of directions) {
    await page.locator(`[data-choice="${d.slug}"]`).click();
    if (await page.locator('.entry:visible').count() !== 1) throw Error('Direction filter failed: ' + d.slug);
    await page.locator(`[id="${d.slug}"]`).screenshot({ path: path.join(base, d.slug, 'brand-board.png') });
  }
  await page.locator('[data-choice="all"]').click();
  if (await page.locator('.entry:visible').count() !== 5) throw Error('All-directions filter failed');
  for (const width of [1360, 768, 390, 320]) {
    await page.setViewportSize({ width, height: 1000 });
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth);
    if (overflow) throw Error('Horizontal overflow at width ' + width);
    await page.locator(`[data-choice="${directions[2].slug}"]`).click();
    await page.locator('[data-choice="all"]').click();
  }
  await page.setViewportSize({ width: 390, height: 1000 });
  await page.locator(`[id="${directions[0].slug}"]`).screenshot({ path: path.join(base, 'mobile-preview.png') });
  await page.setViewportSize({ width: 1360, height: 1000 });
  await page.locator(`[id="${directions[0].slug}"]`).screenshot({ path: path.join(base, 'gallery-preview.png') });
  await page.setViewportSize({ width: 1600, height: 1100 });
  await page.goto(pathToFileURL(path.join(base, 'overview.html')).href);
  await page.evaluate(() => document.fonts.ready);
  await page.locator('img').evaluateAll(imgs => Promise.all(imgs.map(img => img.complete ? Promise.resolve() : new Promise(r => { img.onload = r; img.onerror = r; }))));
  await page.screenshot({ path: path.join(base, 'five-directions.png'), fullPage: true });
  if (errors.length) throw Error(errors.join('\n'));
  console.log('Rendered 5 brand boards. All images/fonts load; filters work; no overflow at 1360, 768, 390 or 320 px.');
  await browser.close();
})().catch(e => { console.error(e); process.exit(1); });
