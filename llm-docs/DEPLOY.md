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
curl -s localhost:8080/healthz       # {"status":"ok","protocol_version":...}
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
   `/deploy/coolify/docker-compose.yaml`.
3. Coolify reads the file and shows the `lazy` service. Set its **domain** (it proposes one; use
   your own, for example `https://chat.example.com`). HTTPS certificates and the proxy's WebSocket
   support are Coolify's; the file routes the domain to port 8080.
4. **Environment Variables.** `SERVICE_PASSWORD_POSTGRES` and `SERVICE_PASSWORD_SETUPCODE` are
   generated for you. **Read `SERVICE_PASSWORD_SETUPCODE` here**: it is the setup code you type when
   you create the owner account. Optional overrides: `LAZY_SEED_REGISTRATION` (`invite_only` or
   `open`), `LAZY_OPEN_WORKSPACE_CREATION`, `LAZY_EVENT_RETENTION_DAYS`, `LAZY_MAX_ATTACHMENT_BYTES`,
   `LAZY_LOG`, `LAZY_LOG_FORMAT`.
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
