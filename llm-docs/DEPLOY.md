# Deploying the Lazy server

The server is one container plus a Postgres. This page covers Docker Compose on any machine, and
Coolify. For the options in detail see `SERVER.md`; for what to try once it is up see `DOGFOOD.md`.

**Status.** The `server` workflow (`.github/workflows/server.yml`, started by hand) has passed on
GitHub's runners: it builds the image, starts the compose stack on a fresh database, runs the smoke
test from outside, and checks that the owner survives a restart of the container and of the whole
stack (run 41155102683, 2026-10-30; see `VERIFICATION.md`). That was plain `http://localhost` on
Linux. Not yet exercised: pushing the image to `ghcr.io`, a real Coolify deployment, an `https://`
address behind a proxy, and a deployment on anyone's own machine. Treat the first real deployment
as the test of this page, and fix the page.

## What runs and what to keep

| Part | What it is | Keep it |
| --- | --- | --- |
| `lazy` | the server (port 8080 in the container) | stateless |
| `postgres` | PostgreSQL 18, database `lazy` | **back it up**: all accounts, messages, and settings |
| `attachments` volume | uploaded files, at `/data/attachments` | **back it up with the database**: the database names the files and the files are the content |

The server applies its migrations when it starts and creates the workspace **Lazy** with `#general`
if there is none. Migrations only go forward: back up before upgrading.

## Safe by default: the setup code

Whoever registers first on a new server becomes the owner of its workspace. On a server that
strangers can reach, that must not be a race. So the deployment files require a **setup code**
(`LAZY_SETUP_CODE`, 20 to 64 letters, digits, `-` and `_`; `openssl rand -hex 24` makes one): until the first account exists, registration needs
the code, typed into the **Invitation code** field of Lazy's first screen (or the **Invitation or
setup code** field of the sign-in card in 0.5.0). Once the
owner exists the code does nothing. The files also seed the workspace as **invitation only**
(`LAZY_SEED_REGISTRATION=invite_only`): everyone after the owner joins with an invitation code from
**Invite people…**. Change either in `.env` (Compose) or the environment variables (Coolify).

## Docker Compose on any machine

You need Docker with the Compose plugin, and a reverse proxy for HTTPS (below) if the machine is
reachable from the internet. The first build compiles the server and takes a few minutes.

```sh
git clone https://github.com/last-refuge/lazy.git && cd lazy
cp .env.example .env
# fill in POSTGRES_PASSWORD and LAZY_SETUP_CODE:  openssl rand -hex 24   (and set LAZY_PUBLIC_URL)
docker compose up -d --build
docker compose ps                    # both healthy
curl -s localhost:8080/healthz       # {"protocol_version":...}
deploy/smoke-test.sh http://localhost:8080 "$(grep ^LAZY_SETUP_CODE .env | cut -d= -f2)"   # optional; creates a throwaway owner, so only on a fresh server
```

Then install the client (`RELEASE.md`), enter the server's address and the setup code on its first
screen, and follow the steps. That account is the owner. Open **Invite people…** to bring in everyone else.

To use a prebuilt image instead of building (see `RELEASE.md`, the `server` workflow):
`LAZY_IMAGE=ghcr.io/last-refuge/lazy/server:<tag> docker compose pull && docker compose up -d --no-build`.

### HTTPS and the reverse proxy

Sessions are bearer tokens: **anything on the public internet must be HTTPS.** Put a reverse proxy
in front and publish only its port. Whatever you use must allow: WebSocket upgrades on `/v1/realtime`
(the client's live connection), request bodies at least as large as `LAZY_MAX_ATTACHMENT_BYTES`
(25 MiB by default), and idle timeouts of several minutes on WebSockets. For example with Caddy,
which does all of that by default:

```
chat.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

and set `LAZY_PUBLIC_URL=https://chat.example.com`. Bind the published port to localhost in that case
(`LAZY_PORT=8080` publishes on every interface; change the `ports` line of `docker-compose.yml` to
`"127.0.0.1:8080:8080"`).

## Coolify

Based on Coolify's documented conventions for Docker Compose applications (the `SERVICE_URL_*` and
`SERVICE_PASSWORD_*` variables); not yet run against a live Coolify.

1. **New Resource**, choose the repository (public: `https://github.com/last-refuge/lazy`, or your
   fork or private copy with a deploy key), branch `main`.
2. **Build Pack: Docker Compose.** Base directory `/`, Docker Compose location
   `/deploy/coolify/docker-compose.yaml`. Keep the base directory `/`: Coolify resolves the compose
   file's relative paths from it, and the file's build context `.` is the repository root only then.
3. Coolify reads the file and shows the `lazy` service. Set its **domain** (it proposes one; use
   your own, for example `https://chat.example.com`). HTTPS certificates and the proxy's WebSocket
   support are Coolify's; the file routes the domain to port 8080.
4. **Environment Variables.** `SERVICE_PASSWORD_POSTGRES` and `SERVICE_PASSWORD_SETUPCODE` are
   generated for you. **Read `SERVICE_PASSWORD_SETUPCODE` here**: it is the setup code you type when
   you create the owner account. Optional overrides: `LAZY_SEED_REGISTRATION` (`invite_only` or
   `open`), `LAZY_OPEN_WORKSPACE_CREATION`, `LAZY_EVENT_RETENTION_DAYS`, `LAZY_MAX_ATTACHMENT_BYTES`,
   `LAZY_LOG`, `LAZY_LOG_FORMAT`. Set **`LAZY_PUBLIC_URL`** to the domain (`https://chat.example.com`):
   without it the server uses Coolify's generated `SERVICE_URL_LAZY`, which can keep an old
   `sslip.io` address after the domain changes (seen on Coolify 4.5.3), and webhook addresses and
   GitHub would use that. The startup log's `public_url=` shows which one it took. For integrations that hold credentials (GitHub), add
   `LAZY_CREDENTIALS_KEY` (see "The credentials key" below).
5. **Deploy.** The first build takes a few minutes. Both services show healthy (the container has
   its own health check, `lazy-server healthcheck`).
6. Install a client, choose `https://chat.example.com` as the server, create the owner account with
   the setup code, and invite everyone else.

Persistent storage: the compose file declares the two volumes (`pgdata`, `attachments`); Coolify
keeps them across deployments. Use Coolify's scheduled backups for Postgres, and back up the
`attachments` volume too (a Coolify scheduled task, or a snapshot of the host).

**Updating.** Redeploy after pushing changes (or enable the webhook for automatic deploys on
your own fork; the repository's own CI is manual-only). Migrations run when the new container starts.

**Alternative: Dockerfile and Coolify's own Postgres.** Create the database as a Coolify resource,
then an application with **Build Pack: Dockerfile** (base directory `/`), port `8080`, a domain, and
a persistent storage mount at `/data/attachments`. Set `DATABASE_URL` to the database's internal
connection URL, `LAZY_PUBLIC_URL` to the domain, and `LAZY_SETUP_CODE` and `LAZY_SEED_REGISTRATION`
as above. Health check path `/healthz`. The server cannot yet talk TLS to Postgres, so use the
internal (non-TLS) URL, not a managed database that requires `sslmode=require`.

## Configuration

All of these are also command-line options (`lazy-server serve --help`).

| Environment | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | required | `postgres://user:password@host:5432/db`; the password must be URL-safe |
| `LAZY_LISTEN` | `0.0.0.0:8080` in the image | address inside the container |
| `LAZY_PUBLIC_URL` | derived | the address clients reach; webhook URLs are shown under it |
| `LAZY_SETUP_CODE` | none | needed to create the first account (20 to 64 letters, digits, `-` and `_`, because it is typed into the app like an invitation code; refused at start otherwise); set it on any reachable server |
| `LAZY_SEED_REGISTRATION` | `open` (`invite_only` in the compose files) | who may register on the workspace the server creates |
| `LAZY_OPEN_REGISTRATION` | `true` | `false` closes registration entirely, invitations included |
| `LAZY_OPEN_WORKSPACE_CREATION` | `true` | whether any signed-in person may create a workspace of their own |
| `LAZY_CREDENTIALS_KEY` | none | 64 hex digits (`openssl rand -hex 32`) that seal integration credentials; without it the server runs and integrations that hold credentials are off |
| `LAZY_CREDENTIALS_OLD_KEYS` | none | previous keys, comma-separated, during a rotation |
| `LAZY_RUN_JOBS` | `true` | run the job worker in the server; set `false` if `lazy-server worker` runs it in a container of its own |
| `LAZY_ATTACHMENTS_DIR` | `/data/attachments` in the image | where uploads are kept (a volume) |
| `LAZY_MAX_ATTACHMENT_BYTES` | 25 MiB | largest upload; the proxy must allow it too |
| `LAZY_EVENT_RETENTION_DAYS` | 30 | days of event history kept (the newest 1,000 per workspace always stay); 0 keeps all |
| `LAZY_LOG`, `LAZY_LOG_FORMAT` | `info,sqlx=warn`, `text` | logging; `json` for machine-readable lines. Nothing logged holds a password, token, invitation code, or message content (`SERVER.md`, Logging) |

## Operating it

- **Logs:** `docker compose logs -f lazy` (or Coolify's Logs tab). Each request line has a request
  id, the route pattern, the status, and the time.
- **Health:** `GET /healthz` answers 200 with the protocol version; clients refuse a server whose
  protocol they do not speak and say so.
- **Backup (Compose):**
  ```sh
  docker compose exec -T postgres pg_dump -U lazy -Fc lazy > lazy-$(date +%F).dump
  docker run --rm -v lazy_attachments:/data -v "$PWD":/backup debian:bookworm-slim \
    tar czf /backup/attachments-$(date +%F).tar.gz -C /data .
  ```
  Take them at about the same time, and keep them somewhere other than this machine.
- **Restore:** a fresh stack, then `docker compose exec -T postgres pg_restore -U lazy -d lazy --clean --if-exists < lazy-DATE.dump`
  and untar the attachments into the `lazy_attachments` volume, then restart `lazy`.
- **Upgrading:** back up, `git pull`, `docker compose up -d --build`. A new server may speak a newer
  protocol than the clients; update clients and server together.
- **Size:** a small team fits in a 1 vCPU, 1 GB machine. Password hashing is deliberately costly
  (memory-hard): many simultaneous sign-ins need more memory.
- **One server process only.** The behavior of several server processes against one database is
  not verified; do not scale `lazy` beyond one replica.
- **Rate limiting** is the proxy's job on a public server.

## Known limits of this deployment

- No TLS between the server and Postgres (same machine or private network only).
- Only the amd64 image is built by the workflow.
- Not exercised yet on a real Coolify; the compose file follows its documented conventions.

## The credentials key

Integrations that act for a workspace (GitHub, and later bot tokens) keep their secrets in the
database sealed with XChaCha20-Poly1305 under `LAZY_CREDENTIALS_KEY`, which is never stored in the
database (D31). A database backup alone is therefore useless to an attacker, and useless to you
without the key: **keep a copy of the key somewhere other than the server and its backups** (a
password manager). If the key is lost, every stored credential is lost and each integration must be
connected again; messages and everything else are unaffected.

- **Set it:** `openssl rand -hex 32`, then in Coolify add `LAZY_CREDENTIALS_KEY` with that value and
  redeploy. The startup log shows `credentials_key=` and 8 hex digits (an id of the key, not the key).
- **Rotate it:** make a new key current, put the old one in `LAZY_CREDENTIALS_OLD_KEYS`, redeploy,
  then run `lazy-server rewrap-credentials` in the container (Coolify: the service's terminal). It
  reports how many it re-sealed; when it reports none sealed by a missing key, remove the old key
  and redeploy.

## Jobs

The server runs background jobs (integration deliveries) from a queue in Postgres, in the server
process by default. To isolate them, run a second container with the same image and environment and
the command `lazy-server worker`, and set `LAZY_RUN_JOBS=false` on the server. Any number of workers
may share the queue.

## Connecting GitHub

Needs `LAZY_CREDENTIALS_KEY` (above) and `LAZY_PUBLIC_URL` set to the `https://` address GitHub can
reach: on Coolify, set it yourself (step 4 above), and check the startup log's `public_url=` before
connecting. An admin then opens **Integrations, GitHub, Connect GitHub**
in the app, names the organization, and follows GitHub's screens: create the app, then install it on
the organization (all repositories, or chosen ones). The app is private to that organization. To
remove it: **Disconnect** in Lazy, then delete the app on GitHub (the organization's settings,
Developer settings, GitHub Apps). Details: D32.

## Connecting Linear, Jira, Sentry and PagerDuty

Needs `LAZY_CREDENTIALS_KEY` and `LAZY_PUBLIC_URL` (as for GitHub). An admin opens **Integrations**, chooses **Linear** or **Jira**, and **Connect**, then follows the steps shown:

- **Linear:** a Linear admin opens Settings, API, Webhooks, **New webhook**, pastes the address Lazy shows, chooses the teams and the resource types (Issues, Comments, Projects, Project updates), saves, and pastes the webhook's signing secret back into Lazy.
- **Jira:** someone with the Administer Jira permission opens Settings, System, WebHooks, **Create a WebHook**, pastes the address, chooses **Make a secret** in Lazy and pastes that into the Secret field, ticks Issue created and updated and Comment created (a JQL filter can narrow it), and saves.

- **Sentry:** a Sentry manager opens Settings, Developer Settings, creates an internal integration, pastes the address as its Webhook URL, turns on Alert Action, gives it Read access to Issue & Event, ticks the issue webhook, and pastes its Client Secret into Lazy. For alerts, add "Send a notification via" the integration to an alert rule.
- **PagerDuty:** a Manager, Admin or Owner opens Integrations, Generic Webhooks (v3), **New Webhook** (or a service's Integrations tab), pastes the address, picks the incident events, and pastes the secret PagerDuty shows (once) into Lazy.

Then **Add a channel** chooses which team (Linear), project (Jira, Sentry) or service (PagerDuty) goes where. Disconnecting in Lazy does not delete the webhook in Linear or Jira; delete it there too.

## Apps and the bot API

An admin makes an app in **Integrations, Apps and bots**: a name and what it may do. Lazy shows its token once; copy it then. Add the bot to channels from the app's screen. A program then posts as the bot:

```sh
curl -X POST https://chat.example.com/v1/bot/messages \
  -H "Authorization: Bearer lzb_..." -H "Content-Type: application/json" \
  -H "Idempotency-Key: deploy-4821" \
  -d '{"channel": "deploys", "text": "Deployed *v1.4.2*"}'
```

The body can also be Slack's (`text` in `mrkdwn`, `blocks`, `attachments`), so code written for Slack's `chat.postMessage` mostly works with the address and token changed. `GET /v1/bot/me` says which app a token is; `GET /v1/bot/conversations` lists its channels. Too many posts too fast answer `429` with `Retry-After`. A lost token cannot be shown again: make a new one in the app's screen.

### Slash commands

On an app's screen, **Add a command**: its name (`deploy` for `/deploy`), the address Lazy sends it to (`https://`), a description, and a usage hint. **Show signing secret** gives the secret your app checks requests with. Lazy signs them exactly as Slack does (`X-Slack-Signature`, `X-Slack-Request-Timestamp`) and sends Slack's form fields, so a Slack command handler works with that secret and its address. Answer within 3 seconds:

- plain text, or `{"text": "..."}`: a reply only the person sees;
- `{"response_type": "in_channel", "text": "..."}`: posted in the conversation by the app's bot;
- an empty `200`, then `POST` to the `response_url` within 30 minutes (up to 5 times) with `"response_type": "in_channel"`.

A private answer sent later through `response_url` is not shown yet; answer privately within the 3 seconds instead. Commands need `LAZY_CREDENTIALS_KEY`, since the signing secret is sealed with it.

### Events

On an app's screen, **Events**: an address and what to send, **Messages that name it** (`@App name` in a message) and/or **Every message in its channels**. Lazy checks the address with Slack's `url_verification` challenge before saving, then sends each event as Slack's `event_callback`, signed like commands. Reply with `POST /v1/bot/messages` using the event's `channel` and its `ts` as `thread_ts`. Messages written by bots are not sent, so two apps never answer each other in a loop.

### Blocks and buttons

Messages from webhooks, the bot API, and app answers can carry Slack's `blocks` (Block Kit): headers, sections with fields and a button, dividers, context lines, and rows of buttons are drawn as such; other kinds are shown as their text. Always send `text` too: older Lazy clients, notifications, and search use it.

A button with a `url` opens it. To be told when someone presses any other button of your app, put an address under **Buttons** on the app's screen. Lazy sends Slack's `block_actions` (a form with one `payload` field) signed like commands, checks on each press that the person may still see the message, and takes the button's `value` from the message itself. Answer within 3 seconds, or later through the `response_url`:

- text: a reply only the person who pressed sees;
- `{"replace_original": true, "text": "...", "blocks": [...]}`: the message is replaced (the buttons with it);
- `{"delete_original": true}`: the message is deleted;
- `{"response_type": "in_channel", "text": "..."}`: a new message from the app.

Webhooks and connectors have no app to answer presses, so only their link buttons are kept.

### Link previews

When someone views a message with links, the server fetches each page (at most 3 per message) and shows its title, description, and site under the message. It needs outbound HTTPS, like GitHub does. It connects only to public addresses on ports 80 and 443 (never this machine, the private network, or a cloud metadata address), follows at most 3 redirects, gives up after 5 seconds, and reads at most 256 KB of HTML. Previews are kept for a day per workspace, and only people who can read the message see them. Pictures from pages are not shown.

### Zapier, n8n and Make

Lazy has no app in their directories yet. Their generic HTTP steps reach everything:

- **Post into Lazy (an action).** Either:
  - use an incoming webhook's address (Integrations, Add incoming webhook) with their Slack-style "send a webhook" step, or a `POST` of `{"text": "..."}`; or
  - make an app and `POST https://your-server/v1/bot/messages` with the header `Authorization: Bearer lzb_...` and a JSON body `{"channel": "deploys", "text": "..."}`, which lets one app post in several channels and reply in threads (`thread_ts`).
- **React to messages (a trigger).**
  1. Make a catch hook in the tool: Zapier's "Webhooks by Zapier, Catch Hook", n8n's Webhook node (POST), or Make's "Custom webhook".
  2. On a Lazy app's screen, paste that address under **Events**, choose the events, and tick **Don't check the address**.
  3. Add the app to the channels to watch.

  Each message arrives as Slack's `event_callback`; the useful fields are `event.text`, `event.user_name`, `event.channel_name`, `event.channel`, and `event.ts` (reply with it as `thread_ts`).

The tool sees every message in those channels, so add the app only where that is wanted.

