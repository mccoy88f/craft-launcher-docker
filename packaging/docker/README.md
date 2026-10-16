# ArtCraft Launcher on your own domain (Docker)

`artcraft-launcher-server` serves the launcher's web page and hosts the Craft Apps' browser
builds next to it, so the same interface the desktop launcher has runs in a browser at
`https://<your domain>/`. The apps themselves are WebAssembly and run on each visitor's own
device: no remote desktop, no VNC, nothing streamed.

| Address | Who | What |
|---|---|---|
| `/` | everyone | The launcher: every app, its MCP status and the settings. Installed apps have an **Open** button that opens the app in a window of its own. |
| `/apps/<app>/` | everyone | An installed app (`/apps/photocraft/`, `/apps/wordcraft/`…). |
| `/admin/login` | the administrator | The launcher's sign-in page (the name and password set in `.env`). Signed in, the same page installs, updates and removes apps and changes the MCP settings. |

Everyone sees the same page. **Install**, **Update**, **Uninstall…**, the MCP switches and the
MCP settings show for visitors too; using one takes a visitor to the sign-in page, and back once
signed in. **Settings → Sign in as administrator** does the same, and **Sign out** ends the
session. The sign-in and OAuth pages speak the language chosen in the launcher's settings (the
page tells the server in an `artcraft_lang` cookie), else the browser's. Visitors see the access tokens' names, never even their last characters. Each app's
details page has a folded **MCP server** button under Website, GitHub and Releases: it unfolds
into the app's status (its browser version, whether its MCP tool is installed and whether MCP is
on), its MCP address, its work folder and, for the administrator, each client's files. **Settings → On this server** lists the same for every
installed app.

## Quick start

You need a machine with Docker, ports 80 and 443 open, and a domain whose DNS points at it.
On Coolify (or another platform with its own HTTPS proxy), see [Coolify](#coolify-traefik)
instead.

```sh
cd packaging/docker
cp .env.example .env          # set DOMAIN and ARTCRAFT_ADMIN_PASSWORD
docker compose up -d --build
```

The first build compiles the launcher in Rust and WebAssembly and takes several minutes. Caddy
gets the HTTPS certificate for `DOMAIN` by itself. Then open `https://<DOMAIN>/admin/login`, sign in,
and install the apps you want. Visitors see them at `https://<DOMAIN>/`.

## Apps in their own window, and installing the launcher (PWA)

**Open** opens each app in a pop-up window as big as the screen: no tabs, no toolbar, no
bookmarks bar. Opening the same app again brings its window back. Browsers still show a slim
strip with the address on pop-ups, for safety. If a pop-up blocker refuses, the app opens in a
tab instead.

The launcher is also a Progressive Web App, with the ArtCraft Launcher icon. Install it from
the browser (Chrome and Edge: the install icon in the address bar, or **Install ArtCraft
Launcher** in the menu; Safari: **File → Add to Dock**; on phones: **Add to Home Screen**).
Installed, it has its own icon and window. The apps are inside its scope (`/apps/…`), so they
open in app windows too, with no browser bars at all.

## MCP: the apps for Claude and other MCP clients

Each Craft App's command-line tool is also an MCP server. The launcher server can run it for
MCP clients (Claude Code, Claude Desktop and others) at `https://<DOMAIN>/mcp/<app>`, over
MCP's Streamable HTTP transport. It's off until the administrator turns it on:

1. **Settings → MCP server → Turn on the MCP server.** Off, no app can be reached.
2. **An app's details → MCP server for <app>.** The first time, the server downloads the
   app's Linux tarball from the same release as the installed version, checks it against the
   published SHA-256 and keeps only `bin/<app>-cli`. Updates of the app update it too.
3. **Settings → New token.** Give it a name (one per client: a computer, a colleague, a script)
   and the apps it may use. The token is shown once, with the command for Claude Code:

   ```sh
   claude mcp add --transport http photocraft https://<DOMAIN>/mcp/photocraft \
     --header "Authorization: Bearer acl_…"
   ```

   Other clients: add a remote MCP server with the app's address and that header. The server
   keeps only each token's SHA-256 fingerprint; **Revoke** ends that client's access and its
   sessions at once.

### claude.ai, Claude Desktop and Claude's mobile apps (OAuth)

Those connect by signing in (OAuth 2.1, as MCP's authorization spec describes), not with a pasted
token:

1. On claude.ai: **Customize → Connectors → Add custom connector** (Team and Enterprise: an owner,
   under **Organization settings → Connectors**). Give it a name and the app's address, for example
   `https://<DOMAIN>/mcp/photocraft`; leave the OAuth fields empty.
2. **Connect** opens the launcher's sign-in page: enter the administrator's name and password and
   choose **Allow**. Claude gets an access token for that app (renewed by itself), listed in
   Settings with Claude's name and "OAuth", and revoked there like the others.
3. The connector then works in Claude on the web, on the desktop and in the mobile apps.

The server finds its public address from the reverse proxy's `X-Forwarded-Proto` and `Host`
headers (Traefik and Caddy send them). If the sign-in page or the addresses in
`/.well-known/oauth-authorization-server` show the wrong address, set `ARTCRAFT_PUBLIC_URL`
(for example `https://launcher.example.com`).

Each client (a token, or a client that signed in with OAuth) has its own work folder in each app,
`/work/<app>/<token id>` (the `launcher-work` volume): its sessions share it, it is the app's only
read and write root, and no other client can see it. The app works on the files there, not on the
documents open in visitors' browsers. Documents a client opens live in its session's memory until
it saves them; a session unused for 30 minutes ends, and what wasn't saved is lost. Files from
before folders were per client are in `/work/<app>/previous`, for the administrator only.

The launcher adds tools of its own to each app's, and tells each client, when it connects (in the
answer to `initialize`), how long files are kept and which of its files expire within two days, so
it can warn its user:

| Tool | What it does |
|---|---|
| `launcher_files_list` | The client's files: path, size, last change, and when each will be deleted. |
| `launcher_file_link` | A link to download one file, valid for an hour (`/files/<random>/<name>`). |
| `launcher_file_from_url` | Only when **Settings → Let MCP clients download files from web addresses** is on (it's off by default): the server downloads a public https address (up to 512 MB, within the client's space) into the client's folder. Addresses on this machine or the local network are refused, redirects included. |

Signed in, the administrator sees every client's files under each app's **MCP server** button:
**Download**, **Delete** (asked twice), and **Add files…** to put files from their device in a
client's folder (up to 512 MB each), for instance a photo for the AI to work on. In Settings:

| Setting | Default | What it does |
|---|---|---|
| Delete files not changed for | 7 days | Once a day, files not changed for that long are deleted (Never: kept). |
| Space per client | 2 GB | Past it, the client's tool calls are refused with a message saying so, until files are deleted or the limit raised. |
| Sessions at once | 10 | MCP sessions running at the same time, across apps. A session nobody uses for 30 minutes ends; when all are taken, a new one closes the one left alone longest (clients like claude.ai often start a new session without ending the last). |
| Empty | | Deletes everything in that app's folder, every client's (ending its sessions). |

**Revoke** (asked twice) ends a token's sessions and deletes its folders.
Uninstalling an app deletes its work folder too (the uninstall prompt says so).

The command-line tool runs inside the launcher's container, as its unprivileged user, with a
clean environment. The MCP server checks the token on every request (sent in the `Authorization`
header, never as a cookie, so a page on another site can't use it), and never runs anything but the tools of the Craft Apps
it installed from their `storytold/` releases.

## When a client can't connect

The server logs why it refused an MCP or OAuth request (Coolify: the service's **Logs**): an unknown
or expired token, a token that may not use that app, an app whose MCP server is off or whose tool
isn't installed, a session that couldn't start, or a refused OAuth token request.
The first lines each app's tool writes about itself are in the log too (`<app> mcp: …`).

The server tries each app's command-line tool once, confined to a work folder
(`--automation-read-root` and `--automation-write-root`; for PdfCraft, its own `--root`, which
confines every read and write too). One that refuses isn't run, since it could reach every file
the server can; the log says what it answered, and the app's **MCP server** box says **Not yet
compatible with MCP**. Its next release that takes the options works as soon as the app is updated.
As of October 2026:

| | Apps |
|---|---|
| Work | PhotoCraft, VectorCraft, EffectCraft, WordCraft, SoundCraft, PdfCraft |
| Release not compatible, source code is: build from source (below) | DeckCraft, DesignCraft |
| Not compatible yet | FilmCraft, LightCraft, GridCraft, CadCraft, FigCraft |

### Building an MCP tool from source

For an app whose release isn't compatible but whose code on `main` is (the list is
`FROM_SOURCE` in `apps/artcraft-launcher-server/src/builds.rs`), its **MCP server** box offers
**Build the MCP tool from source**. The build service compiles it:

- **`cli-builder`** is a second service in the compose files (`packaging/builder/`), on the
  official Rust image. It sees only the `launcher-builds` volume it shares with the launcher, never
  the launcher's data or tokens, and clones only `https://github.com/storytold/<app>`. Idle
  otherwise.
- A build runs `cargo build --release --locked -p <app>-cli` at the latest commit of `main`:
  5-20 minutes, about 3 GB of disk while it runs (cleaned up afterwards) and about 1 GB of memory
  per job (`ARTCRAFT_BUILD_JOBS`, 2 by default).
- The launcher takes the result in by itself, runs it only if it confines itself like a release's
  tool, and shows **MCP tool built from source (commit)**. **Build again** fetches newer code.
- Once an update brings a release whose own tool is compatible, the launcher uses that and deletes
  the one built from source.

Without the `cli-builder` service, the box says it's needed. Remove the service (and the
`launcher-builds` lines) if you don't want it.

## Settings

| Variable | Default | What it does |
|---|---|---|
| `DOMAIN` | (required) | The domain Caddy serves and gets a certificate for. |
| `ARTCRAFT_ADMIN_PASSWORD` | (required by the compose file) | The administrator's password, at least 8 characters. Without one the server still runs, but nobody can install, update or remove apps. |
| `ARTCRAFT_ADMIN_PASSWORD_FILE` | | Read the password from a file instead (Docker secrets). |
| `ARTCRAFT_ADMIN_USER` | `admin` | The administrator's name. |
| `ARTCRAFT_CHECK_MINUTES` | `30` | How often the server looks for new releases. The administrator's Refresh button looks at once (at most once a minute). |
| `ARTCRAFT_LAUNCHER_GITHUB_TOKEN` | | Optional GitHub token (no scopes needed): 5000 API requests an hour instead of 60. Only ever sent to `api.github.com`. |
| `ARTCRAFT_PUBLIC_URL` | (from the proxy's headers) | The server's public address, for OAuth (`https://launcher.example.com`). |
| `ARTCRAFT_LISTEN` | `0.0.0.0:8080` | Where the server listens inside the container. |
| `ARTCRAFT_DATA_DIR` | `/data` | Installed apps, MCP settings and tokens, saved release answers and downloads (the `launcher-data` volume). |
| `ARTCRAFT_WORK_DIR` | `/work` | The apps' MCP work folders (the `launcher-work` volume). |
| `ARTCRAFT_BUILDS_DIR` | `/builds` | Shared with the build service, for MCP tools built from source (the `launcher-builds` volume). |
| `ARTCRAFT_BUILD_JOBS` | `2` | On `cli-builder`: parallel compile jobs (about 1 GB of memory each). |
| `ARTCRAFT_WEB_DIR` | `/srv/artcraft-launcher` | The launcher's web build, put there by the image. |
| `ARTCRAFT_LOG` | `info` | `error`, `warn`, `info`, `debug` or `trace`. |

## How installing works

1. The server looks up each app's latest release on GitHub, once for every visitor (answers
   are revalidated with ETags and saved in the data volume).
2. **Install** downloads that release's `<app>-web-<version>.zip`, only from
   `https://github.com/storytold/<app>/releases`. The download resumes after a dropped
   connection and can be paused.
3. The zip is checked against the SHA-256 that GitHub or the release's `SHA256SUMS.txt`
   publishes. Without a published checksum, nothing is installed.
4. It is unpacked into `/data/apps/<app>/<version>-<n>/`. Paths that would land outside that
   folder, links, and archives over 2 GB or 50,000 files are refused.
5. `installed.json` switches over to the new folder in one rename, so visitors never see half an
   app. The previous version's folder is kept until the next update, for pages still loading it.

An app whose release has no browser build shows **Not available**.

## Data and privacy

Each app runs in the visitor's browser and keeps its preferences there (`localStorage`). Files
are opened with the browser's file picker and saved as downloads. Nothing a visitor makes is
sent to the server. The server stores only the apps it installed and the release information.

All apps and the launcher share the domain's origin, so they share the browser's storage for
that domain. They are all Craft Apps from `storytold/`, which is why the server hosts nothing
else.

## Security notes

- Use HTTPS: the sign-in page sends the administrator's password, and the session cookie
  (`HttpOnly`, `SameSite=Lax`, `Secure` on HTTPS) travels with every request. The compose file
  puts Caddy in front for that; Coolify's Traefik does the same. Scripts may send the name and
  password with HTTP Basic instead. Browsers also turn on WebGPU and clipboard
  access only on HTTPS pages.
- Each wrong password costs a second, which makes guessing slow. Choose a long password anyway.
- Every request that changes something needs the `x-artcraft-launcher` header. A form on another
  site can't send it, so another page can't make a signed-in administrator's browser install or
  remove an app.
- The container runs as an unprivileged user (uid 10001) and only writes to `/data`.

## Coolify (Traefik)

Coolify's Traefik already provides HTTPS, so neither the launcher nor Caddy has to get a
certificate. Use the `docker-compose.yml` at the repository root, which has no Caddy and
publishes no ports:

1. **New resource** → your repository (and branch) → Build Pack **Docker Compose**, compose file
   `/docker-compose.yml`.
2. On the `launcher` service, set the domain (for example `https://launcher.example.com`).
   Coolify routes it to the container's port 8080 and gets the certificate.
3. Under **Environment Variables**, set `ARTCRAFT_ADMIN_PASSWORD` (at least 8 characters), and
   optionally `ARTCRAFT_ADMIN_USER`, `ARTCRAFT_CHECK_MINUTES` and `ARTCRAFT_LAUNCHER_GITHUB_TOKEN`.
4. Deploy. The `launcher-data` volume keeps the installed apps across deploys.

The Dockerfile build pack works too: point it at `/Dockerfile`, set the port to `8080`, add a
persistent storage mounted at `/data`, and set the same variables.

Traefik passes the `Authorization` header through and leaves the apps' files as they are, so
nothing else needs configuring.

## Without Caddy

To use a reverse proxy you already run (nginx, Traefik…), remove the `caddy` service and publish
the launcher on loopback with `ports: ["127.0.0.1:8080:8080"]`, then proxy your domain to it.
Pass the `Authorization` header through, and don't let the proxy add `Content-Encoding` to an
app's `app/*.part-*` files: they are gzip already, and the app inflates them itself.

Any path works: every URL the pages use is relative, so a proxy can also serve everything under a
prefix like `https://example.com/launcher/`. Redirect `/launcher` to `/launcher/` and strip the
prefix when proxying.

## Without Docker

```sh
(cd apps/artcraft-launcher-web && trunk build --release)
cargo build --release -p artcraft-launcher-server
ARTCRAFT_WEB_DIR=dist/web ARTCRAFT_DATA_DIR=./data ARTCRAFT_ADMIN_PASSWORD='…' \
  ARTCRAFT_LISTEN=127.0.0.1:8080 target/release/artcraft-launcher-server
```
