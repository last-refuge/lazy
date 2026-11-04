// The Android app on a booted emulator with the debug APK installed, against a real server that has
// a person `ada`: real taps and the real back button through adb, with the page read over the debug
// build's DevTools port. Each check prints ok or FAIL; the exit code is the number that failed.
//   scripts/android.sh test
// LAZY_SLOW stretches every wait, for an emulator without hardware acceleration.
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";

const server = process.env.LAZY_SERVER ?? "http://10.0.2.2:8080";
const shots = process.env.LAZY_SHOTS ?? "/tmp/lazy-android-shots";
const slow = Number(process.env.LAZY_SLOW ?? 1);
const app = "io.github.last_refuge.lazy";
const message = "Hello from the Android app";
mkdirSync(shots, { recursive: true });

let bad = 0;
const check = (label, ok, extra = "") => {
  if (!ok) bad++;
  console.log(ok ? "ok  " : "FAIL", label, extra);
};
const adb = (...args) => execFileSync("adb", args, { encoding: "utf8", timeout: 120000 * slow, stdio: ["ignore", "pipe", "ignore"] });
const pause = (ms) => new Promise((done) => setTimeout(done, ms * slow));
async function until(what, test, seconds = 60) {
  const end = Date.now() + seconds * 1000 * slow;
  for (;;) {
    try {
      const value = await test();
      if (value) return value;
    } catch {}
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((done) => setTimeout(done, 1000));
  }
}
const finds = (what, test, seconds) => until(what, test, seconds).then(() => true, () => false);

// ----- Native screens, from the system's view of them -----
function nodes() {
  adb("shell", "rm", "-f", "/sdcard/u.xml");
  try {
    adb("shell", "uiautomator", "dump", "/sdcard/u.xml");
  } catch (error) {
    // Some images kill uiautomator (status 137) right after it has written the file.
    if (!String(error.stdout).includes("dumped to")) throw error;
  }
  return [...adb("shell", "cat", "/sdcard/u.xml").matchAll(/<node [^>]*>/g)].map((m) => {
    const get = (name) => m[0].match(new RegExp(`${name}="([^"]*)"`))?.[1] ?? "";
    const b = get("bounds").match(/\d+/g)?.map(Number) ?? [0, 0, 0, 0];
    return { text: get("text"), desc: get("content-desc"), cls: get("class"), x: (b[0] + b[2]) / 2, y: (b[1] + b[3]) / 2, top: b[1] };
  });
}
const tapAt = (x, y) => adb("shell", "input", "tap", String(Math.round(x)), String(Math.round(y)));
// Ignores case: the system theme decides whether a button's label is in capitals, so it varies by device.
const native = (text) => until(`"${text}" on screen`, () => nodes().find((n) => [n.text, n.desc].some((t) => t.toLowerCase() === text.toLowerCase())));
const appOnTop = () => (adb("shell", "dumpsys", "activity", "activities").match(/topResumedActivity=.*/)?.[0] ?? "").includes(app);
const press = (...keys) => adb("shell", "input", "keyevent", ...keys);

// A stopped run says what was on the device, which goes away with the emulator.
process.on("uncaughtException", (error) => {
  try {
    console.log("On screen:", nodes().map((n) => n.text || n.desc).filter(Boolean).join(" | "));
    writeFileSync(`${shots}/android-stopped.png`, execFileSync("adb", ["exec-out", "screencap", "-p"], { timeout: 30000, maxBuffer: 1 << 26, stdio: ["ignore", "pipe", "ignore"] }));
    console.log(`Screenshot: ${shots}/android-stopped.png`);
  } catch {}
  console.error(error);
  process.exit(1);
});

// ----- The page, over the debug port (Playwright cannot attach to a web view this way) -----
async function connect() {
  const pid = await until("the app's process", () => adb("shell", "pidof", app).trim());
  await until("the debug port", () => adb("shell", "cat", "/proc/net/unix").includes(`webview_devtools_remote_${pid}`), 120);
  adb("forward", "tcp:9333", `localabstract:webview_devtools_remote_${pid}`);
  const target = await until("the page", async () => {
    const list = await (await fetch("http://127.0.0.1:9333/json/list")).json();
    return list.find((t) => t.url.includes("/web"));
  }, 120);
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((open, fail) => {
    socket.onopen = open;
    socket.onerror = fail;
  });
  const waiting = new Map();
  let next = 0;
  socket.onmessage = (event) => {
    const reply = JSON.parse(event.data);
    waiting.get(reply.id)?.(reply);
    waiting.delete(reply.id);
  };
  const send = (method, params = {}) =>
    new Promise((answer, fail) => {
      const id = ++next;
      const timer = setTimeout(() => fail(new Error(`${method} got no answer`)), 90000 * slow);
      waiting.set(id, (reply) => {
        clearTimeout(timer);
        answer(reply.result);
      });
      socket.send(JSON.stringify({ id, method, params }));
    });
  const run = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? "script failed");
    return result.result.value;
  };
  return {
    run,
    close: () => socket.close(),
    shows: (selector) => run(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); if (!e) return false; const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0; })()`),
    save: async (name) => writeFileSync(`${shots}/android-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).data, "base64")),
    // A real tap on the screen, where the element is. `offset` is the web view's top on the screen.
    tap: async (selector, offset) => {
      const at = await run(`(() => { const e = [...document.querySelectorAll(${JSON.stringify(selector)})].pop(); e.scrollIntoView({ block: "nearest" }); const r = e.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2, dpr: devicePixelRatio }; })()`);
      tapAt(at.x * at.dpr, at.y * at.dpr + offset);
    },
    fill: (selector, text) =>
      run(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); e.focus(); e.value = ${JSON.stringify(text)}; e.dispatchEvent(new Event("input", { bubbles: true })); })()`),
  };
}

// ----- First run -----
adb("forward", "--remove-all");
adb("shell", "pm", "clear", app);
adb("shell", "am", "start", "-n", `${app}/.MainActivity`);
const field = await native("Server address");
check("first run asks for the server", true);
tapAt(field.x, field.y);
await pause(1000);
adb("shell", "input", "text", "not%sa%sserver");
// A keyboard opening moves the button, so it is looked for after typing.
let button = await native("Connect");
tapAt(button.x, button.y);
await until("a refusal", () => nodes().some((n) => /Could not reach|Enter an address/.test(n.text)), 40);
check("a bad address is refused", true);
press("KEYCODE_MOVE_END", ...Array(14).fill("KEYCODE_DEL"));
adb("shell", "input", "text", server);
button = await native("Connect");
tapAt(button.x, button.y);

// ----- The page -----
let page = await connect();
await until("the sign-in form", () => page.shows("input[name=username]"), 180);
check("the server's page loads", true);
check("the page knows it is in the app", (await page.run("navigator.userAgent")).includes("LazyShell/1"));
check("sign-in offers Change server", await page.shows("[data-action=change-server]"));
const web = await until("the web view on screen", () => nodes().find((n) => n.cls === "android.webkit.WebView"));
const top = web.top;
check("the page starts below the status bar", top > 0, `at ${top}px`);
await page.fill("input[name=username]", "ada");
await page.fill("input[name=password]", "correct horse battery");
await page.save("1-signin");
await page.tap("#signin-submit", top);
await until("the conversation", () => page.shows("#draft"), 120);
check("a real tap signs in", true);
await pause(1000);
check("no offer of browser notifications, which a web view cannot show", !(await page.run(`document.body.innerText.includes("Turn on")`)));
await page.save("2-conversation");

// ----- The keyboard, on images that have one -----
if (adb("shell", "ime", "list", "-s").trim()) {
  const tall = await page.run("innerHeight");
  await page.tap("#draft", top);
  const opened = await finds("the keyboard", () => page.run(`innerHeight < ${tall} - 100`), 40);
  check("the keyboard makes the page shorter", opened);
  await pause(500);
  const fit = await page.run(`(() => ({ view: visualViewport.height, composer: document.querySelector(".composer-wrap").getBoundingClientRect().bottom, header: document.querySelector(".header").getBoundingClientRect().top }))()`);
  check("the composer stays above the keyboard, and the header on screen", fit.composer <= fit.view + 1 && fit.header === 0, JSON.stringify(fit));
  await page.save("2b-keyboard");
  adb("shell", "input", "text", "typed%son%sthe%skeyboard");
  check("the keyboard types into the composer", await finds("the text", () => page.run(`document.querySelector("#draft").value === "typed on the keyboard"`), 30));
  press("KEYCODE_BACK");
  await finds("the keyboard to close", () => page.run(`innerHeight >= ${tall} - 1`), 30);
  check("the back button closes the keyboard, not the app", appOnTop());
} else {
  console.log("note this image has no keyboard: not checked");
}

// ----- Messaging -----
await page.fill("#draft", message);
await page.tap("#composer .send", top);
const sent = `[...document.querySelectorAll("#rows .msg")].filter((m) => m.textContent.includes(${JSON.stringify(message)}))`;
check("a real tap on Send sends", await finds("the message", () => page.run(`${sent}.length > 0 && !${sent}.pop().classList.contains("pending")`), 60));
await pause(500);
await page.tap("#rows .msg .msg-text", top);
check("a real tap on a message shows its actions", await finds("the actions", () => page.shows(".msg.picked .msg-actions"), 30));
await page.save("3-actions");

// ----- The back button -----
await page.tap(".msg.picked [data-action=open-thread]", top);
await until("the thread", () => page.shows("#thread-draft"), 40);
await page.save("4-thread");
press("KEYCODE_BACK");
check("the back button closes the thread", (await finds("the thread to close", async () => !(await page.shows("#thread-draft")), 40)) && appOnTop());
await page.tap(".menu-toggle", top);
await until("the sidebar", () => page.shows(".shell.sidebar-open"), 30);
await pause(500);
await page.save("5-sidebar");
press("KEYCODE_BACK");
check("the back button closes the sidebar", (await finds("the sidebar to close", async () => !(await page.shows(".shell.sidebar-open")), 40)) && appOnTop());
press("KEYCODE_BACK");
check("the back button with nothing open leaves the app", await finds("the app to leave", () => !appOnTop(), 40));

// ----- Coming back -----
adb("shell", "am", "start", "-n", `${app}/.MainActivity`);
check("the app comes back", await finds("the app", () => appOnTop(), 40));
check("the page survives leaving and coming back", await finds("the page", () => page.run(`${sent}.length > 0`), 60));
page.close();
adb("shell", "am", "force-stop", app);
adb("forward", "--remove-all");
adb("shell", "am", "start", "-n", `${app}/.MainActivity`);
page = await connect();
check("after a force-stop the server and the session are remembered", await finds("the conversation", () => page.shows("#draft"), 180));
check("the sent message is there", await finds("the message", () => page.run(`${sent}.length > 0`), 60));
page.close();
adb("forward", "--remove-all");
console.log(bad ? `${bad} failed` : "all ok");
process.exit(bad);
