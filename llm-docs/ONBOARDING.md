# First run: from install to "hello"

Status: **the invitee's and the owner's paths are written; none of it has been compiled or run** (see
"What is built" at the end and `VERIFICATION.md`). Agreed direction (2026-10-29): no invitation links and no central
directory. Whoever runs a server hands people two things, the server's address (a domain or an IP)
and an invitation code. The person installs Lazy, enters them, and Lazy validates both, remembers
the server, and walks them through creating their account. For many people this is the first
impression of Lazy, so it is designed as a short, beautiful sequence, not a form.

## What is true today (read from the code, 2026-10-29)

- The first screen is the sign-in card. Its **Server** row is at the *bottom* and is pre-filled with
  `http://127.0.0.1:8080`, the developer default, so an installed copy opens on "Can't reach the
  server" until the person works out what to type. The invitation code is the last, optional field
  of "Create account".
- Registering is one request: username, display name, password, and the code together. Nothing
  validates a code before that request, so the app cannot say "you're invited to X" first, and a bad
  code is found out only after the person has chosen a password.
- An invitation code was 192 bits as 32 URL-safe characters (`aB3-x_…`, case-sensitive, with `-` and
  `_`); since D29 a new one is 20 characters of an alphabet without lookalikes. It is hard to read out, hard to type, and easy to get wrong (`l`, `I`, `1`, `O`, `0`).
- `/healthz` returns only the protocol version. The client cannot tell an open server from an
  invitation-only one, or one that is waiting for its first owner, before registration fails.
- The seeded workspace is named "Lazy". The first owner has to rename it in workspace settings.
- One profile talks to one server. A person can be in many workspaces *on that server*; two servers
  need two profiles.
- Moss has four poses (`primary`, `welcome`, `caught-up`, `celebrate`); the brand sets onboarding
  size at 160 to 320 px. The app embeds them at 512 px and prepares them at 256, which is too small
  for that. The kit's masters are 1254 px. `celebrate` is not used anywhere yet.
- Typed in by hand, an `http://` address to a public IP sends the password in the clear. Nothing
  tells the person.

## The two stories

**The person who runs the server** (owner) installs the server (`DEPLOY.md`), opens Lazy, enters the
server's address and the setup code, and becomes the owner. Lazy then asks what the workspace is
called and ends by showing exactly what to hand out: a ready-to-send message with the address and a
first invitation code. The owner's first run ends with the thing the next person needs.

**Everyone else** (invitee) receives that message, installs Lazy, enters the address and the code,
sees the workspace they were invited to, chooses a name, a username and a password, optionally adds a
picture, and arrives in `#general`.

A returning person on a new computer enters the address, their username, and their password.

## The sequence

Five screens for an invitee, each asking one thing. The window is split on wide windows (a Forest
panel with Moss and the tagline on the left, the step on the right) and stacks on narrow ones (Moss
at 160 px above the step). A quiet "Step 2 of 4" tells the person how far there is to go. Escape or a
"Back" link goes back; Enter goes forward; the first field of each step has the keyboard.

| # | Screen | Asks | What Lazy does |
| --- | --- | --- | --- |
| 1 | **Welcome to Lazy.** Connect to your team's server. | Server address; invitation code | Checks the address as the person types (reachable, same protocol), then checks the code. Nothing is created. Saves the server on success. |
| 2 | **You're invited to Zebrafish Collective.** *Quentin Fakename invited you. 12 people are already here.* | Nothing: **Join workspace** | Shows who and what the code is for. This is also the person's proof they are at the right server before they give it a password. Moss `welcome`, large. |
| 3 | **Let's set you up.** | Your name; username (suggested from the name); password | The one request that creates the account (`register`). Password: show/hide, "At least 8 characters. A short phrase works well." No complexity rules. |
| 4 | **Add a picture.** *So people know it's you.* | A picture (optional) | Live preview in the avatar circle with initials as the fallback; **Choose a picture**, **Skip for now**. Uses the profile calls that exist. |
| 5 | **You're all set, Quentin.** *Say hello in #general.* | Nothing: **Open Lazy** | Moss `celebrate`. Lands in `#general` with the composer focused. One line: "Press Ctrl+K any time to jump to a person or channel." |

The owner's variant replaces 2 with **Set up your workspace** (the code is the setup code) and adds
after 4: **Name your workspace** (renames "Lazy") and **Invite your team**: the first invitation
code, and a **Copy message** button that copies:

```
Join us on Lazy.

1. Get Lazy: https://github.com/last-refuge/lazy/releases/latest
2. Server: chat.acme.com
3. Invitation code: K7QM-4TXD-9BWP-H2VA-ZX3C

The code works for 10 people until 5 November.
```

The same message is behind a **Copy invitation** button in Invite people, so an owner never has to
assemble it from pieces.

Brand rules that apply (D26): Moss is never in an error, security, or outage state, so "can't reach
the server" shows the logo; Moss never speaks in the first person; all copy is sentence case and
comes from `VOICE-AND-COPY.md` where a pattern exists ("You're invited.", "Welcome to your
workspace."); motion is 120 to 240 ms and none of it loops; reduced motion is honored.

### What can go wrong, and what Lazy says

| Situation | Says |
| --- | --- |
| Address does not parse | The existing messages (`ServerUrlError::message`). |
| Nothing answers | "Can't reach this server. Check the address and your connection." (shown inline, with the logo, not Moss) |
| Answers but is not Lazy | "That address doesn't look like a Lazy server." |
| Protocol newer than this Lazy | "This server needs a newer Lazy." and, when the updater can, **Update now**. |
| Protocol older | "This server runs an older version of Lazy. Ask whoever runs it to update." |
| Code is wrong, expired, revoked, or used up | One message for all four, as the server already does: "That invitation code isn't valid. It may have expired or been used. Ask for a new one." |
| `http://` to a host that is not on a private network | A calm notice, not a block: "This server isn't encrypted. Your password could be seen on the way. Continue only if you trust the network." (Some people will run on a bare IP; that is their call.) |
| Username taken | "That username is taken. Try quentin2." (suggestions computed here, not by asking the server which are free) |
| Password under 8 characters | "Use at least 8 characters." |
| The request takes longer than 10 s | "That's taking a while. Check the address." and the button comes back. |
| Server is an old build without the new endpoints | Step 2 is skipped; the code is checked when the account is created, as today. |

Every status is words plus a mark (check, warning), never color alone; errors are announced to screen
readers; no message repeats what the person typed (it may hold a code).

## Work

### A. Server and wire (no `PROTOCOL_VERSION` bump: both additions are new routes)

1. **Public server info**, `GET /v1/server` (or extra fields on `/healthz`): `protocol_version`,
   `registration` (`open` or `invite_only`), and `setup_required` (the workspace has no member yet).
   No names: a server nobody has been invited to tells a stranger nothing.
2. **Preview an invitation**, `POST /v1/invitations/preview` with `{code}` (POST so the code is never
   in a URL or a log): for a valid code, `{kind: invitation | setup, workspace_name,
   inviter_display_name, member_count}`; for anything else the same `invitation_invalid` as
   registration, with the same message. Consumes nothing. Names are revealed only for a valid code.
3. **A code a person can read.** New invitations are 20 Crockford base32 characters (100 bits; no
   `I L O U`, case-insensitive), shown as five groups of four. Registration and accept normalize case,
   spaces, and hyphens; the 32-character codes already issued keep working (the shape check already
   allows 20 to 64). 100 bits makes guessing pointless without a rate limiter; a per-address limiter
   is still worth adding for the cost of requests, and is its own task.
4. **Invite people** gains **Copy invitation** (the message above, built from the address the client
   already uses).

### B. Client core and runtime

5. An **onboarding state** in `lazy-client-core` (pure, testable, reusable by a phone client):
   `Connect → Invited → Account → Picture → Done`, with the owner's extra steps, driven by inputs
   (`CheckServer`, `PreviewInvitation`, `Register`, …) and effects the runtime already half has
   (`CheckServer`, `Register`) plus `PreviewInvitation`. The invitation code lives in memory only,
   redacted in `Debug`, read in the same few places the source-count tests allow.
6. **Remember the server at step 1**, not at registration (the saved-server file exists), so quitting
   after step 1 does not lose it. The code is not saved; the account is created minutes later in the
   same sitting. The session is saved at step 3, so quitting at 4 or 5 still leaves a signed-in
   person who can add a picture from Preferences.
7. **Code input that forgives**: trims, drops spaces and hyphens, folds case and `O/I/L` for the new
   format, and never alters a 32-character code. A pasted "address then code" block fills both
   fields.
8. **Release builds start with no server chosen**; the localhost default stays for development
   (`run-client.sh` already sets `LAZY_SERVER_URL`).

### C. UI (`lazy-ui`)

9. `OnboardingView` replaces the sign-in card for joining; the sign-in card remains, restyled, for
   returning people, reached from step 1 ("Already have an account? Sign in").
10. **Layout**: the split brand panel, step indicator, the single-column form at a comfortable
    measure (about 420 px), Nunito Bold display headings, DM Sans body.
11. **Moss at the brand's onboarding size**: embed the kit's 1254 px masters (resized to 768 px at
    build time, about 1 MB each for the three poses used) and prepare them at 2x display size at
    start, as `brand.rs` does for the others.
12. **Motion**: a small helper for 160 to 240 ms entrances (fade and a short rise), off under reduced
    motion. GPUI has `with_animation` at the pinned revision; whether it respects the system's
    reduced-motion setting needs checking.
13. **Accessibility**: step title announced on entry, labels and live validation messages, focus
    order, Enter and Escape, text sizes that survive the window being small, a dark-theme pass.
14. The owner's steps and the **Copy message** button.

### D. Proving it

15. Tests: onboarding state transitions in the core (every error row above); the server's preview and
    info routes and the new code format in the in-process server tests; the runtime against a stub;
    UI tests per step; the existing e2e that register through the UI move to the new path through one
    helper (this is the biggest churn).
16. **Look at it**: `screens` scenes for every step, light and dark, 1200 and 640 px wide, the owner's
    variant, and each error row. Plan on at least two rounds of looking and adjusting; I cannot see
    any of it without a `screens` run, so batch the scenes.
17. **Walk it on real machines** with a fresh server: Windows, macOS, and Linux, from a clean
    install to a first message, written down in `VERIFICATION.md`. Include a screen reader and a
    keyboard-only pass. These are the owner's to do.

### Order

Server and wire (A) and the core (B) first: they are testable without seeing anything. Then the UI
(C) with its scenes, reviewed together. Then D. The code format (A3) and Copy invitation (A4) are
small and useful on their own, so they can ship before the screens.

## Decisions to make

1. **The shorter code format (A3).** Recommended: yes. Easy to read aloud and to type, safe at 100
   bits, and nothing already issued breaks.
2. **Is the owner's branch in the first version?** Recommended: yes. It is two screens, and it is
   what makes the next person's first run work.
3. **Step 2 as its own screen.** Recommended: yes. It is the emotional beat, and it is what shows a
   person they reached the right server.
4. **Plain `http://`:** warn and allow (recommended), or block outside private networks.
5. **Version bump.** Recommended: none; a new client against an old server skips step 2.
6. **Copy.** The words above are drafts in the brand's voice for you to rewrite.

## Not in this plan

A central directory of servers, invitation links, and deep links (`lazy://`): the first two are not
wanted, and the last is easy to add later because a link would only carry the same two values.
Hosted accounts, email sign-in, and password reset are Phase 6 and later. Code signing is its own
track (`RELEASE.md`): the SmartScreen and Gatekeeper warnings come *before* this sequence and are the
other big first-impression cost.

## What is built (2026-10-29, written and not run)

Server and wire (A1 to A3 and the Copy message of A4): `GET /v1/server` and
`POST /v1/invitations/preview` (`lazy-server/src/onboarding.rs`, tests in `tests/onboarding.rs`);
new codes of 20 characters in Crockford's base32, shown in five groups, with every reader forgiving
case, groups, dashes, and `O`/`I`/`L` (`lazy_domain::workspace::canonical_invite_code`, used by
registration, accepting, and previewing; older 32-character codes still work and are never
rewritten); **Copy message to send** in Invite people. No `PROTOCOL_VERSION` bump.

Client core and runtime (B): `Input::ServerFacts`, `Input::PreviewInvitation`,
`Input::InvitationPreviewed`, `Effect::PreviewInvitation`, `ClientCore::server_facts()` and
`invitation_check()`; the runtime asks `/v1/server` just before it answers a check that passed, and
maps a refusal of the preview to `Invalid`, `Closed`, `Unsupported` (an older server), or `Failed`.
The existing server switch already saves the address when the check passes, so the address is
remembered at step one without new code. Deviation from the plan: the *step* state lives in the
view (`lazy-ui/src/onboarding.rs`), not the core; the core holds what the server said, which is
what a phone client would need.

UI (C): `OnboardingView` with the five screens, the Forest brand panel on wide windows and a stacked
layout on narrow ones, Moss at 288 px from new 768 px pictures, entrance animations that GPUI skips
under reduced motion, the username suggested from the name, a taken name offered a way out, and
`screens` scenes `first-01` to `first-16`. It is **opt-in**: `lazy_ui::onboarding::enable(cx)`
(called by the desktop) makes a new install open on it; without that call, and for anyone whose
server was chosen before or fixed by `LAZY_SERVER_URL`, the sign-in card is first as it always was,
so the existing tests and `scripts/run-client.sh` are unchanged. The sign-in card gained "New here?
Join with an invitation" and the first screen "I already have an account".

**The owner's two extra screens (written, not run).** A code that previews as `setup` makes the
person the owner: the heading of the second screen is "Set up your workspace.", the steps are counted
to six, and after the picture come **Name your workspace** (renames the seeded "Lazy"; Continue is off
until there is a name that can be used, a refusal is said under the field, Skip keeps "Lazy", and a
name equal to the current one is not sent) and **Invite your team**. The second asks for the first
invitation by itself (7 days, up to 25 people) and shows what will be sent: where to get Lazy, the
server, the code in groups in one selectable field that cannot be edited (the only place it is
read is `dialogs::shown_code`, which the invitations dialog uses too), and for how long and how
many; **Copy message to send** puts the whole message on the clipboard, Continue forgets the code
(it is shown once, and the note says more can be made in Invite people), and a failure to make the
invitation is said with Try again. The first screen counts "Step 1" without a total, because a
person who has not been checked yet might be either.

**Setup codes the client can send (written, not run).** A setup code is typed into the same field as
an invitation code, so the server now refuses to start with one that is not 20 to 64 letters, digits,
`-` and `_` (the message says so, does not repeat it, and says to remove the variable when people have
signed up already, since it then does nothing). It also compares the code in the form the app sends it
(`canonical_invite_code` on both sides, padding still refused), so a setup code of 20 hexadecimal
digits works however it was configured. The old minimum was 16; a server whose setup code was 16 to
19 characters, or had other characters (`openssl rand -base64` output), will not restart until the
variable is changed or removed.

## Not built yet

- **A warning for `http://` to a host that is not on a private network.**
- **A show/hide button on the password** (the field's masking is fixed when it is made).
- **A limit on how often a stranger may ask** (`/v1/server`, the preview, and registration have
  none; the 100-bit code is what protects it).
- **Walking it on real machines** and the screen reader and keyboard-only passes (the owner's).

