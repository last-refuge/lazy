// The web client (D47) at phone size with a touchscreen (D49), in headless Chromium against a real
// server: what scripts/test-web.sh runs after flow.mjs. Each check prints ok or FAIL; the exit code
// is the number that failed. Chromium cannot show a phone's keyboard, so a shorter window stands in
// for one: that takes the same path through --viewport-height, but it is not a real keyboard.
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";
const base = process.env.LAZY_WEB_URL ?? "http://127.0.0.1:18080";
const out = process.env.LAZY_WEB_SHOTS ?? "/tmp/lazy-web-shots";
mkdirSync(out, { recursive: true });
const password = "correct horse battery";
// Their own people: flow.mjs changes Ada's password.
for (const name of ["pia", "pat"]) {
  await fetch(`${base}/v1/auth/register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ username: name, display_name: `${name} Tester`, password }),
  });
}
const browser = await chromium.launch({ channel: "chromium" });
let bad = 0;
const check = (label, ok, extra = "") => { if (!ok) bad++; console.log(ok ? "ok  " : "FAIL", label, extra); };
const c = await browser.newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true, colorScheme: "dark" });
const p = await c.newPage();
p.on("pageerror", (e) => check("page error", false, e.message));
p.on("console", (m) => { if (/Content Security|violat/i.test(m.text())) check("CSP", false, m.text()); });
p.on("dialog", (d) => d.accept());
await p.goto(base + "/");
await p.fill("input[name=username]", "pia");
await p.fill("input[name=password]", password);
await p.tap("#signin-submit");
await p.waitForSelector("#draft", { timeout: 15000 });
check("touch media query", await p.evaluate(() => matchMedia("(hover: none)").matches));
check("--viewport-height set under the real CSP", (await p.evaluate(() => document.documentElement.style.getPropertyValue("--viewport-height"))) === "844px");
const sent = await p.locator("#rows .msg").count();
for (let i = 1; i <= 24; i++) { await p.fill("#draft", `Message number ${i} with enough words to look like a real line of chat on a phone screen`); await p.keyboard.press("Enter"); await p.waitForFunction((n) => document.querySelectorAll("#rows .msg").length >= n, sent + i); }
await p.locator("#rows .msg:not(.pending)").last().waitFor();
await p.evaluate(() => { const t = document.getElementById("timeline"); t.scrollTop = t.scrollHeight; });
await p.waitForTimeout(200);
const metrics = () => p.evaluate(() => { const t = document.getElementById("timeline"); const r = (s) => document.querySelector(s)?.getBoundingClientRect(); return { vw: innerWidth, sw: document.documentElement.scrollWidth, scrollY, bottomGap: t.scrollHeight - t.scrollTop - t.clientHeight, composerBottom: r(".composer-wrap")?.bottom, headerTop: r(".header")?.top }; });
let m = await metrics();
check("no sideways overflow", m.sw <= m.vw, JSON.stringify(m));
await p.screenshot({ path: `${out}/phone-1-timeline.png` });
const rows = p.locator("#rows .msg");
const n = await rows.count();
const last = rows.nth(n - 1), prev = rows.nth(n - 3);
const left = (l) => l.locator(".msg-text").evaluate((e) => e.getBoundingClientRect().left);
const before = await left(last);
check("actions hidden before a tap", !(await last.locator(".msg-actions").isVisible()));
await last.locator(".msg-text").tap();
check("no tap highlight", (await p.evaluate(() => getComputedStyle(document.documentElement).webkitTapHighlightColor)) === "rgba(0, 0, 0, 0)");
check("tap shows actions", await last.locator(".msg-actions").isVisible());
check("text stays put when picked", (await left(last)) === before, `${before} -> ${await left(last)}`);
const ab = await last.locator(".msg-actions").boundingBox(), tl = await p.locator("#timeline").boundingBox(), tx = await last.locator(".msg-text").boundingBox();
check("actions under the text, inside the list", ab.y >= tx.y + tx.height && ab.y + ab.height <= tl.y + tl.height + 0.5, JSON.stringify({ ab, tx, tl }));
const sizes = await last.locator(".msg-actions button").evaluateAll((b) => b.map((x) => { const r = x.getBoundingClientRect(); return [Math.round(r.width), Math.round(r.height)]; }));
check("action buttons finger-sized", sizes.length > 0 && sizes.every(([w, h]) => w >= 40 && h >= 40), JSON.stringify(sizes));
await p.screenshot({ path: `${out}/phone-2-picked.png` });
await prev.locator(".msg-text").tap();
check("tap another moves it", (await prev.locator(".msg-actions").isVisible()) && !(await last.locator(".msg-actions").isVisible()));
await prev.locator(".msg-text").tap();
check("tap again hides it", !(await prev.locator(".msg-actions").isVisible()));
await last.locator(".msg-text").tap();
await p.locator(".header").tap({ position: { x: 200, y: 25 } });
check("tap elsewhere hides it", !(await last.locator(".msg-actions").isVisible()));
await last.locator(".msg-text").tap();
await last.locator(".msg-actions [data-action=react]").tap();
await p.waitForSelector("#picker-body .pick");
const box = await p.locator(".picker").boundingBox();
check("picker on screen", box.x >= 0 && box.x + box.width <= 390 && box.y >= 0 && box.y + box.height <= 844, JSON.stringify(box));
await p.screenshot({ path: `${out}/phone-3-picker.png` });
await p.locator("#picker-body .pick").first().tap();
await last.locator(".reaction").first().waitFor();
check("reaction added by touch", true);
// A keyboard: Chromium cannot show one, so the window shrinks instead (same variable, same path).
await p.locator("#draft").tap();
check("composer field is 16px (no iOS zoom)", (await p.locator("#draft").evaluate((e) => getComputedStyle(e).fontSize)) === "16px");
await p.evaluate(() => { const t = document.getElementById("timeline"); t.scrollTop = t.scrollHeight; });
await p.waitForTimeout(100);
await p.setViewportSize({ width: 390, height: 480 });
await p.waitForTimeout(200);
m = await metrics();
check("short viewport: composer on screen, header at top, still at newest", m.composerBottom <= 480 && m.headerTop === 0 && m.bottomGap <= 40, JSON.stringify(m));
await p.screenshot({ path: `${out}/phone-4-keyboard.png` });
await p.setViewportSize({ width: 390, height: 844 });
await p.waitForTimeout(200);
// Safe areas, if this Chromium can emulate them.
const cdp = await c.newCDPSession(p);
let insets = true;
try { await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 47, bottom: 34, left: 0, right: 0 } }); } catch (e) { insets = false; console.log("note safe-area emulation unavailable:", e.message.split("\n")[0]); }
await p.waitForTimeout(200);
if (insets) {
  const pad = await p.evaluate(() => ({ head: getComputedStyle(document.querySelector(".header")).paddingTop, wrap: getComputedStyle(document.querySelector(".composer-wrap")).paddingBottom }));
  check("safe areas respected", pad.head === "57px" && pad.wrap === "34px", JSON.stringify(pad));
  await p.screenshot({ path: `${out}/phone-5-notch.png` });
}
await last.locator(".msg-text").tap();
await last.locator(".msg-actions [data-action=open-thread]").tap();
await p.waitForSelector("#thread-draft");
const th = await p.locator(".thread").boundingBox();
check("thread covers the screen", th.x === 0 && th.width === 390 && th.height === 844, JSON.stringify(th));
await p.screenshot({ path: `${out}/phone-6-thread.png` });
await p.setViewportSize({ width: 390, height: 480 });
await p.waitForTimeout(200);
const tb = await p.locator("#thread-draft").boundingBox();
check("thread composer above a keyboard", tb.y + tb.height <= 480, JSON.stringify(tb));
await p.setViewportSize({ width: 390, height: 844 });
await p.locator(".thread-head button").last().tap();
await p.waitForTimeout(150);
await p.locator(".menu-toggle").tap();
await p.waitForTimeout(350);
await p.screenshot({ path: `${out}/phone-7-sidebar.png` });
const sb = await p.locator(".sidebar").boundingBox();
check("sidebar slides in, full height", sb.x === 0 && sb.height === 844, JSON.stringify(sb));
// Inside a phone app's shell (D49), which says so in its user agent.
const sc = await browser.newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true, userAgent: `${await p.evaluate(() => navigator.userAgent)} LazyShell/1` });
const s = await sc.newPage();
s.on("pageerror", (e) => check("shell page error", false, e.message));
await s.goto(base + "/");
check("shell: sign-in offers Change server", await s.locator("[data-action=change-server]").isVisible());
await s.fill("input[name=username]", "pia");
await s.fill("input[name=password]", password);
await s.tap("#signin-submit");
await s.waitForSelector("#draft", { timeout: 15000 });
await s.waitForTimeout(300);
const back = () => s.evaluate(() => window.lazyBack());
check("shell: back with nothing open leaves the app", (await back()) === false);
await s.locator(".menu-toggle").tap();
await s.waitForTimeout(300);
await s.locator("[data-action=account-menu]").tap();
const offered = s.locator("#popover [data-action=change-server]");
await offered.waitFor({ timeout: 3000 }).catch(() => {});
check("shell: the account menu offers Change server", await offered.isVisible());
const closes = async (label, gone) => {
  const answered = await back();
  await s.waitForTimeout(300);
  check(`shell: back closes ${label}`, answered === true && (await gone()));
};
await closes("the menu", async () => !(await offered.isVisible()));
await closes("the sidebar", async () => (await s.locator(".shell.sidebar-open").count()) === 0);
const tail = s.locator("#rows .msg").last();
await tail.locator(".msg-text").tap();
await tail.locator("[data-action=open-thread]").tap();
await s.waitForSelector("#thread-draft");
await closes("the thread", async () => !(await s.locator("#thread-draft").isVisible()));
// A picture opens over the page instead of replacing it.
const png = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M/wHwAEBgIApD5fRAAAAABJRU5ErkJggg==", "base64");
const chooser = s.waitForEvent("filechooser");
await s.locator("#composer [data-action=attach]").tap();
await (await chooser).setFiles({ name: "dot.png", mimeType: "image/png", buffer: png });
await s.waitForTimeout(800);
await s.fill("#draft", "a picture");
await s.locator("#composer .send").tap();
const link = s.locator("a.image").last();
await link.waitFor({ timeout: 15000 });
const pageUrl = s.url();
await link.tap();
await s.waitForSelector("#lightbox img");
check("shell: a picture opens over the page", s.url() === pageUrl && (await s.locator("#lightbox .lightbox-save").getAttribute("download")) !== null);
check("shell: the picture loads", await s.locator("#lightbox img").evaluate((i) => i.decode().then(() => i.naturalWidth > 0, () => false)));
await s.screenshot({ path: `${out}/phone-9-picture.png` });
check("shell: back closes the picture", (await back()) === true && (await s.locator("#lightbox").count()) === 0);
await link.tap();
await s.locator("#lightbox img").tap();
check("shell: a tap closes the picture", (await s.locator("#lightbox").count()) === 0);
// The popover stays when only the height changes: a phone's keyboard, opening for its search field.
await s.locator("#rows .msg").last().locator(".msg-text").tap();
await s.locator("#rows .msg").last().locator("[data-action=react]").tap();
await s.waitForSelector("#picker-body .pick");
await s.setViewportSize({ width: 390, height: 500 });
await s.waitForTimeout(200);
check("shell: a keyboard opening leaves the picker open", (await s.locator(".picker").count()) === 1);
const pb = await s.locator(".picker").boundingBox();
await s.screenshot({ path: `${out}/phone-10-picker-keyboard.png` });
check("shell: the picker fits above the keyboard", pb.y >= 0 && pb.y + pb.height <= 500, JSON.stringify(pb));
await s.setViewportSize({ width: 844, height: 390 });
await s.waitForTimeout(200);
check("shell: turning the phone closes it", (await s.locator(".picker").count()) === 0);
await sc.close();
// In a browser none of that shows.
check("browser: no Change server", (await p.locator("[data-action=change-server]").count()) === 0);
// The desktop is unchanged.
const dc = await browser.newContext({ viewport: { width: 1280, height: 800 } });
const d = await dc.newPage();
await d.goto(base + "/");
await d.fill("input[name=username]", "pat");
await d.fill("input[name=password]", password);
await d.click("#signin-submit");
await d.waitForSelector("#rows .msg", { timeout: 15000 });
const row = d.locator("#rows .msg").last();
await row.hover();
check("desktop: hover shows actions", await row.locator(".msg-actions").isVisible());
await row.locator(".msg-text").click();
check("desktop: click picks nothing", (await d.locator(".msg.picked").count()) === 0);
await row.hover();
const bh = await row.locator(".msg-actions button").first().evaluate((e) => e.getBoundingClientRect().height);
check("desktop: button sizes unchanged", bh > 0 && bh < 40, String(bh));
await d.screenshot({ path: `${out}/phone-8-desktop.png` });
await browser.close();
console.log(bad ? `${bad} failed` : "all ok");
process.exit(bad);
