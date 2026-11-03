// Two people use the web client (D47) in headless Chromium against a real server: what
// scripts/test-web.sh runs. Each step prints ok or FAIL; the exit code is the number that failed.
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";
const base = process.env.LAZY_WEB_URL ?? "http://127.0.0.1:18080";
const shots = process.env.LAZY_WEB_SHOTS ?? "/tmp/lazy-web-shots";
const files = process.env.LAZY_WEB_FILES;
mkdirSync(shots, { recursive: true });
let failed = 0;
// The full Chromium (in its new headless mode): the headless shell denies notifications outright.
const browser = await chromium.launch({ channel: process.env.LAZY_WEB_BROWSER ?? "chromium" });
const logs = [];
const answers = [];
async function person(name, password = "correct horse battery") {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  await context.grantPermissions(["notifications"], { origin: base });
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() !== "info") logs.push(`${name} ${m.type()}: ${m.text()}`); });
  page.on("pageerror", (e) => logs.push(`${name} pageerror: ${e.message}`));
  page.on("response", (r) => { if (r.status() >= 400) logs.push(`${name} ${r.status()} ${r.request().method()} ${new URL(r.url()).pathname}`); });
  // Confirmations are accepted; a prompt gets the answer the step put in `answers`.
  page.on("dialog", (d) => d.accept(d.type() === "prompt" ? answers.shift() ?? d.defaultValue() : undefined));
  await page.goto(base + "/");
  await page.fill("input[name=username]", name);
  await page.fill("input[name=password]", password);
  await page.click("#signin-submit");
  if (password === "correct horse battery") {
    await page.waitForSelector("#sidebar .conv.active", { timeout: 15000 });
  }
  return page;
}
const step = async (label, fn) => {
  try { await fn(); console.log("ok  ", label); }
  catch (e) { failed += 1; console.log("FAIL", label, "-", e.message.split("\n")[0]); }
};
const rows = (page) => page.locator("#rows");
const ada = await person("ada");
const bob = await person("bob");

await step("Ada sends; Bob sees it live", async () => {
  await ada.fill("#draft", "Plan for **today**: ship the web client");
  await ada.keyboard.press("Enter");
  await rows(bob).getByText("ship the web client").waitFor({ timeout: 8000 });
});
await step("Bob reacts with the picker; Ada sees it", async () => {
  const row = bob.locator(".row", { hasText: "ship the web client" });
  await row.hover();
  await row.locator(".actions [data-action=react]").click();
  await bob.fill("#picker-q", "thumbs up");
  await bob.locator("#picker-body .pick").first().click();
  await ada.locator(".row", { hasText: "ship the web client" }).locator(".reaction").first().waitFor({ timeout: 8000 });
  const text = await ada.locator(".row", { hasText: "ship the web client" }).locator(".reaction").first().textContent();
  if (!text.includes("1")) throw new Error("reaction count: " + text);
});
await step("Ada replies in a thread; Bob sees the reply count", async () => {
  const row = ada.locator(".row", { hasText: "ship the web client" });
  await row.hover();
  await row.locator(".actions [data-action=open-thread]").click();
  await ada.waitForSelector("#thread:not([hidden])");
  await ada.fill("#thread-draft", "first step: the server serves it");
  await ada.keyboard.press("Enter");
  await ada.locator("#thread-rows").getByText("first step").waitFor({ timeout: 8000 });
  await bob.locator(".thread-link", { hasText: "1 reply" }).waitFor({ timeout: 8000 });
  await ada.screenshot({ path: `${shots}/10-thread.png` });
  await ada.click("[data-action=close-thread]");
});
await step("Ada edits her message; Bob sees (edited)", async () => {
  const row = ada.locator("#rows .row", { hasText: "ship the web client" });
  await row.hover();
  await row.locator(".actions [data-action=edit]").click();
  await ada.waitForFunction(() => document.querySelector("#draft").value.includes("ship the web client"));
  await ada.fill("#draft", "Plan for **today**: ship the web client, then test it");
  await ada.keyboard.press("Enter");
  await bob.locator("#rows .row", { hasText: "then test it" }).locator(".edited").waitFor({ timeout: 8000 });
});
await step("Bob starts a DM with Ada; Ada sees it with a presence dot", async () => {
  await bob.click("[data-action=new-message]");
  await bob.fill("#people-q", "ada");
  await bob.locator("[data-action=pick-person]", { hasText: "Ada" }).click();
  await bob.click("[data-action=start-conversation]");
  await bob.waitForFunction(() => document.querySelector("#header h1")?.textContent.includes("Ada"));
  await bob.fill("#draft", "hi Ada, this is a DM");
  await bob.keyboard.press("Enter");
  await ada.locator("#sidebar .conv", { hasText: "Bob" }).waitFor({ timeout: 8000 });
  await ada.locator("#sidebar .conv", { hasText: "Bob" }).locator(".dot.online").waitFor({ timeout: 8000 });
  await ada.screenshot({ path: `${shots}/11-dm-arrived.png` });
  await ada.locator("#sidebar .conv", { hasText: "Bob" }).click();
  await rows(ada).getByText("this is a DM").waitFor({ timeout: 8000 });
});
await step("Ada sends an image and a file; Bob sees the thumbnail and the file", async () => {
  await ada.locator("#sidebar .conv", { hasText: "general" }).click();
  const chooser = ada.waitForEvent("filechooser");
  await ada.click("#composer [data-action=attach]");
  await (await chooser).setFiles([`${files}/red.png`, `${files}/notes.txt`]);
  await ada.locator("#chips .chip", { hasText: "red.png" }).waitFor();
  await ada.waitForFunction(() => [...document.querySelectorAll("#chips .chip-state")].every((e) => !/Reading|Uploading/.test(e.textContent)), null, { timeout: 15000 });
  await ada.fill("#draft", "two files");
  await ada.keyboard.press("Enter");
  await bob.locator("#sidebar .conv", { hasText: "general" }).click();
  await bob.locator(".row", { hasText: "two files" }).locator(".files img").waitFor({ timeout: 10000 });
  await bob.locator(".row", { hasText: "two files" }).locator("a.file", { hasText: "notes.txt" }).waitFor();
  const loaded = await bob.locator(".row", { hasText: "two files" }).locator(".files img").evaluate((img) => img.complete && img.naturalWidth > 0);
  if (!loaded) throw new Error("thumbnail did not load");
  await bob.screenshot({ path: `${shots}/12-files.png` });
});
await step("Ada creates a channel; Bob finds and joins it", async () => {
  await ada.click("[data-action=browse]");
  await ada.click("[data-action=create-channel]");
  await ada.fill("#create-channel-form input[name=name]", "plans");
  await ada.fill("#create-channel-form input[name=topic]", "What we're doing next");
  await ada.click("#create-channel-form button[type=submit]");
  await ada.waitForFunction(() => document.querySelector("#header h1")?.textContent.includes("#plans"));
  await ada.fill("#draft", "welcome to plans");
  await ada.keyboard.press("Enter");
  await bob.click("[data-action=browse]");
  await bob.fill("#browse-q", "pla");
  await bob.locator("#dlg-body [data-action=join]").first().click();
  await bob.waitForFunction(() => document.querySelector("#header h1")?.textContent.includes("#plans"), null, { timeout: 8000 });
  await rows(bob).getByText("welcome to plans").waitFor({ timeout: 8000 });
});
await step("Bob searches and opens a hit", async () => {
  await bob.click("[data-action=open-search]");
  await bob.fill("#search-q", "ship");
  await bob.keyboard.press("Enter");
  await bob.locator("#dlg-body .hit").first().waitFor({ timeout: 8000 });
  await bob.screenshot({ path: `${shots}/13-search.png` });
  await bob.locator("#dlg-body .hit").first().click();
  await bob.waitForFunction(() => document.querySelector("#header h1")?.textContent.includes("#general"), null, { timeout: 8000 });
});
await step("Jump to (Ctrl+K)", async () => {
  await bob.keyboard.press("Control+k");
  await bob.fill("#switcher-q", "plan");
  await bob.keyboard.press("Enter");
  await bob.waitForFunction(() => document.querySelector("#header h1")?.textContent.includes("#plans"), null, { timeout: 8000 });
});
await step("Suggestions: @mention and :emoji complete in the composer", async () => {
  await ada.locator("#sidebar .conv", { hasText: "general" }).click();
  await ada.locator("#draft").click();
  await ada.keyboard.type("hey @bo");
  await ada.locator("#suggest .list-main", { hasText: "Bob" }).waitFor({ timeout: 5000 });
  await ada.screenshot({ path: `${shots}/16-suggest.png` });
  await ada.keyboard.press("Enter");
  await ada.keyboard.type("nice :tad");
  await ada.locator("#suggest .list-main").first().waitFor({ timeout: 5000 });
  await ada.keyboard.press("Tab");
  const value = await ada.inputValue("#draft");
  if (value !== "hey @bob nice 🎉") throw new Error("composer holds: " + JSON.stringify(value));
  await ada.keyboard.press("Enter");
  await bob.locator("#sidebar .conv", { hasText: "general" }).click();
  await bob.locator("#rows .row.mentioned", { hasText: "nice 🎉" }).waitFor({ timeout: 8000 });
});
await step("Bob sets the topic of #plans from the channel menu", async () => {
  await bob.locator("#sidebar .conv", { hasText: "plans" }).click();
  await bob.click("#header [data-action=conversation-menu]");
  answers.push("Ship it this week");
  await bob.click("#popover [data-action=edit-topic]");
  await ada.locator("#sidebar .conv", { hasText: "plans" }).click();
  await ada.locator("#header .hd-topic", { hasText: "Ship it this week" }).waitFor({ timeout: 8000 });
});
await step("Notifications: offered only without permission, and Turn on asks the browser", async () => {
  if (await ada.locator("[data-action=notifications-on]").count()) throw new Error("offered with permission");
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  await page.goto(base + "/");
  await page.fill("input[name=username]", "bob");
  await page.fill("input[name=password]", "correct horse battery");
  await page.click("#signin-submit");
  await page.locator("[data-action=notifications-on]").waitFor({ timeout: 10000 });
  await page.screenshot({ path: `${shots}/21-notifications-offer.png` });
  // Turn on asks the browser (headless, it answers by itself) and the offer goes away.
  await page.click("[data-action=notifications-on]");
  await page.locator("[data-action=notifications-on]").waitFor({ state: "detached", timeout: 8000 });
  await context.close();
});
await step("Ada deletes a message; Bob no longer sees it", async () => {
  await ada.locator("#sidebar .conv", { hasText: "plans" }).click();
  const row = ada.locator(".row", { hasText: "welcome to plans" });
  await row.hover();
  await row.locator(".actions [data-action=delete]").click();
  await rows(bob).getByText("welcome to plans").waitFor({ state: "detached", timeout: 8000 });
});
await step("Unread: Bob's message to #general shows in Ada's sidebar", async () => {
  await bob.locator("#sidebar .conv", { hasText: "general" }).click();
  await bob.fill("#draft", "@ada are you there?");
  await bob.keyboard.press("Enter");
  await ada.locator("#sidebar .conv.unread", { hasText: "general" }).waitFor({ timeout: 8000 });
  await ada.locator("#sidebar .conv", { hasText: "general" }).locator(".badge").waitFor({ timeout: 8000 });
  await ada.screenshot({ path: `${shots}/14-unread.png` });
  await ada.locator("#sidebar .conv", { hasText: "general" }).click();
  await ada.locator(".row.mentioned", { hasText: "are you there" }).waitFor({ timeout: 8000 });
  await ada.locator("#sidebar .conv.unread", { hasText: "general" }).waitFor({ state: "detached", timeout: 8000 });
});
await step("Reload keeps Ada signed in", async () => {
  await ada.reload();
  await ada.waitForSelector("#sidebar .conv.active", { timeout: 15000 });
});
await step("Ada signs out: back to sign-in, nothing kept", async () => {
  await ada.click("[data-action=workspace-menu]");
  await ada.click("#popover [data-action=sign-out]");
  await ada.waitForSelector("#signin-form", { timeout: 8000 });
  const kept = await ada.evaluate(() => Object.keys(localStorage).filter((k) => k.startsWith("lazy.")));
  if (kept.length) throw new Error("left behind: " + kept);
  const cookies = await ada.context().cookies();
  if (cookies.some((c) => c.name === "lazy_session" && c.value)) throw new Error("cookie kept");
  await ada.reload();
  await ada.waitForSelector("#signin-form", { timeout: 8000 });
});
await step("A wrong password says so", async () => {
  const stranger = await person("ada", "not the password");
  await stranger.locator("#signin-status .error").waitFor({ timeout: 8000 });
  await stranger.screenshot({ path: `${shots}/19-wrong-password.png` });
  await stranger.context().close();
});
await step("Cy creates an account in the browser", async () => {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const cy = await context.newPage();
  cy.on("pageerror", (e) => logs.push(`cy pageerror: ${e.message}`));
  await cy.goto(base + "/");
  await cy.click("[data-action=show-register]");
  await cy.fill("#register-form input[name=display_name]", "Cyrus Newcomer");
  await cy.fill("#register-form input[name=username]", "cy");
  await cy.fill("#register-form input[name=password]", "correct horse battery");
  await cy.click("#signin-submit");
  await cy.locator("#signin-status .error").waitFor({ timeout: 8000 });
  await cy.screenshot({ path: `${shots}/20-register-refused.png` });
  await cy.fill("#register-form input[name=username]", "cyrus");
  await cy.click("#signin-submit");
  await cy.waitForSelector("#sidebar .conv.active", { timeout: 15000 });
  await context.close();
});
await bob.screenshot({ path: `${shots}/15-bob.png` });
await step("A phone-sized window: the sidebar slides over", async () => {
  await bob.setViewportSize({ width: 390, height: 760 });
  await bob.waitForTimeout(400);
  await bob.screenshot({ path: `${shots}/17-phone.png` });
  await bob.click(".menu-toggle");
  await bob.waitForTimeout(400);
  await bob.screenshot({ path: `${shots}/18-phone-sidebar.png` });
  await bob.locator("#sidebar .conv", { hasText: "plans" }).click();
  await bob.waitForFunction(() => !document.querySelector("#shell").classList.contains("sidebar-open"));
});
console.log(logs.length ? "LOGS\n" + logs.join("\n") : "no console errors");
await browser.close();
process.exit(failed + (logs.some((line) => line.includes("pageerror")) ? 1 : 0));
