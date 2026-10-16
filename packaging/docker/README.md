# ArtCraft Launcher on your own domain (Docker)

`artcraft-launcher-server` serves the launcher's web page and hosts the Craft Apps' browser
builds next to it, so the same interface the desktop launcher has runs in a browser at
`https://<your domain>/`. The apps themselves are WebAssembly and run on each visitor's own
device: no remote desktop, no VNC, nothing streamed.

| Address | Who | What |
|---|---|---|
| `/` | everyone | The launcher. Installed apps have an **Open** button that opens the app in a window of its own. |
| `/apps/<app>/` | everyone | An installed app (`/apps/photocraft/`, `/apps/wordcraft/`…). |
| `/admin/` | the administrator | The same launcher, with **Install**, **Update to…**, **Update all** and **Uninstall…**. The browser asks for the name and password set in `.env`. |

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
gets the HTTPS certificate for `DOMAIN` by itself. Then open `https://<DOMAIN>/admin/`, sign in,
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

## Settings

| Variable | Default | What it does |
|---|---|---|
| `DOMAIN` | (required) | The domain Caddy serves and gets a certificate for. |
| `ARTCRAFT_ADMIN_PASSWORD` | (required by the compose file) | The administrator's password, at least 8 characters. Without one the server still runs, but nobody can install, update or remove apps. |
| `ARTCRAFT_ADMIN_PASSWORD_FILE` | | Read the password from a file instead (Docker secrets). |
| `ARTCRAFT_ADMIN_USER` | `admin` | The administrator's name. |
| `ARTCRAFT_CHECK_MINUTES` | `30` | How often the server looks for new releases. The administrator's Refresh button looks at once (at most once a minute). |
| `ARTCRAFT_LAUNCHER_GITHUB_TOKEN` | | Optional GitHub token (no scopes needed): 5000 API requests an hour instead of 60. Only ever sent to `api.github.com`. |
| `ARTCRAFT_LISTEN` | `0.0.0.0:8080` | Where the server listens inside the container. |
| `ARTCRAFT_DATA_DIR` | `/data` | Installed apps, saved release answers and downloads (the `launcher-data` volume). |
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

- Use HTTPS: the administrator's password travels with every request under `/admin/`. The
  compose file puts Caddy in front for that. Browsers also turn on WebGPU and clipboard
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
