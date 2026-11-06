# Handoff: automated server deploys

Status of the GitHub Actions → GHCR → Cloudflare Tunnel → `deploy.sh`
pipeline, and the steps still needed before merging `client-expansion` into
`main` turns it on. Reference docs: `DEV-USAGE-AND-DEPLOYMENT.md` §2.8 (how
it works, setup) and §2.1 (tunnel and Access).

Last updated 2026-10-18.

## How it works

1. CI passes on a push to `main`, which triggers `.github/workflows/deploy.yml`.
2. **publish** builds `server/Dockerfile` and pushes it to the private package
   `ghcr.io/sterance/delegation-server`, tagged `:<sha>` and `:latest`.
3. **deploy** runs in the `production` environment. It SSHes to ANTON
   through the delegation Cloudflare Tunnel (Access service token) and runs
   `server/deploy.sh deploy <sha>`, with the job's short-lived
   `GITHUB_TOKEN` on stdin for the image pull.
4. `deploy.sh` pulls the image, switches to it, health-checks `/clients`,
   and rolls back automatically if the check fails.

The deploy key is a forced command in `authorized_keys`, so it can only run
`deploy.sh deploy <sha> | rollback | status`.

## Done

- [x] Code on `client-expansion`: `deploy.yml`, `server/deploy.sh`, compose
      changes (`image:` with `DELEGATION_IMAGE`/`DELEGATION_TAG`, cloudflared
      `extra_hosts`), docs §2.8.
- [x] Cloudflare Access app `delegation-ssh` for `ssh.smith-c.com`, with
      policy action **Service Auth** (it was Allow, which caused the browser
      login) and the service token `github-deploy`.
- [x] Public hostname `ssh.smith-c.com` → SSH `host.docker.internal:22`
      (currently on the **wrong tunnel**, see the blocker below).
- [x] ANTON (`~/delegation`): checked out `client-expansion`, tagged the
      running image `server-delegation-server:latest` as
      `delegation-server:latest` (the first deploy's rollback target),
      recreated only cloudflared, added the forced-command deploy key to
      `authorized_keys`. `server/deploy.sh status` works locally.
- [x] Deploy key generated on the laptop at `~/.ssh/delegation_deploy`.

## Blocker: the delegation stack is a second replica of the ANTON tunnel

`server/.env` uses the **ANTON** tunnel's token (the one shared with the
self-hosted services). The ANTON tunnel therefore has 2 replicas:

| Replica hostname (container ID) | What it is | Can reach |
|---|---|---|
| `d5f4788bf2a4` | Self-hosting cloudflared | `192.168.50.2:*` only |
| `a3751358274f` | `server-cloudflared-1` (delegation) | `192.168.50.2:*`, `delegation-server`, `host.docker.internal` |

Cloudflare sends requests to either one. Requests for `ssh.smith-c.com`
(and some for `llm.smith-c.com`) land on `d5f4788bf2a4`, which can't reach
those origins. That gives you:

- `curl` returns `error code: 502`.
- The ssh test fails with `websocket: bad handshake`.
- Nothing appears in the delegation cloudflared logs.

**Constraint:** the self-hosted services on ANTON are the top priority and
must not be disturbed. The delegation stack must stay self-contained (its own
cloudflared and tunnel), so it can also be deployed to a blank server.

## Remaining steps

### 1. Give delegation its own tunnel

1. On ANTON, confirm the container IDs. `a3751358274f` must be
   `server-cloudflared-1`. Don't touch the other one.
   ```bash
   docker ps --format '{{.ID}}  {{.Names}}' | grep -E 'd5f4788bf2a4|a3751358274f'
   ```
2. Dashboard: **Networks → Tunnels → Create a tunnel → Cloudflared**, name
   it `delegation`, environment Docker. Copy **only the token**. Don't run the
   `docker run` command it shows, and skip adding routes.
3. On ANTON, switch the delegation connector to the new tunnel:
   ```bash
   cd ~/delegation/server
   cp .env .env.bak
   sed -i 's|^TUNNEL_TOKEN=.*|TUNNEL_TOKEN=<new delegation token>|' .env
   docker compose up -d --no-deps cloudflared
   docker compose logs --tail 20 cloudflared    # "Registered tunnel connection"
   ```
   Check in the dashboard: ANTON has 1 replica (`d5f4788bf2a4`) and
   delegation has 1 replica.
4. Move the routes. `llm` and `ssh` will be down for about a minute; the
   home services are unaffected.
   1. **ANTON → Routes:** delete only `llm.smith-c.com` and
      `ssh.smith-c.com`.
   2. **DNS → Records** (`smith-c.com`): make sure no `llm` or `ssh` CNAME is
      left. Leave the other five.
   3. **delegation → Routes → Add route → Published application:**
      - `llm.smith-c.com` → `http://delegation-server:7070`
      - `ssh.smith-c.com` → type **SSH**, `host.docker.internal:22`
5. Verify: ANTON shows 1 replica and 5 routes; delegation shows 1 replica and
   2 routes. A couple of home services still load, and
   `https://llm.smith-c.com/` still shows the Access login, then the
   dashboard. The Access apps are tied to hostnames, so they need no changes.

### 2. End-to-end SSH test (laptop)

```bash
ssh -i ~/.ssh/delegation_deploy \
  -o ProxyCommand="cloudflared access ssh --hostname %h --service-token-id <ID> --service-token-secret <SECRET>" \
  sterance@ssh.smith-c.com status
```

- Expected: `[deploy] deployed: delegation-server:latest`, followed by the
  `docker compose ps` table.
- Running the same command with `bash` instead of `status` must print the
  usage error and exit, with no shell.
- If it fails, run `docker compose logs -f --tail 0 cloudflared` on ANTON
  during the attempt. The error now shows up there (see the
  `DEV-USAGE-AND-DEPLOYMENT.md` troubleshooting table and §2.8 for the sshd
  and firewall checks).
- Once it works: `rm ~/delegation/server/.env.bak` (it holds the old token).

### 3. GitHub `production` environment

**Settings → Environments → New environment** `production`. Set
**Deployment branches** to **Selected branches** and add `main`. Optionally,
add yourself as a required reviewer.

| Kind | Name | Value |
|---|---|---|
| Secret | `DEPLOY_SSH_KEY` | Full contents of laptop `~/.ssh/delegation_deploy` (private key) |
| Secret | `CF_ACCESS_CLIENT_ID` | `github-deploy` service token Client ID |
| Secret | `CF_ACCESS_CLIENT_SECRET` | `github-deploy` service token Client Secret |
| Variable | `DEPLOY_HOST` | `ssh.smith-c.com` |
| Variable | `DEPLOY_USER` | `sterance` |
| Variable | `DEPLOY_HOST_KEY` | `ssh-ed25519 AAAA…` from ANTON's `/etc/ssh/ssh_host_ed25519_key.pub` (no hostname, no comment) |

ANTON is `linux/amd64`, so `DEPLOY_PLATFORMS` isn't needed.

### 4. Merge and watch the first deploy

1. Merge `client-expansion` into `main`. CI runs; when it passes,
   **Actions → Deploy** starts by itself.
2. Check that the publish job created the package as **Private** (your
   profile → Packages → `delegation-server`).
3. The deploy job's log should end with `[deploy] healthy:
   ghcr.io/sterance/delegation-server:<sha>`. If the health check fails,
   the server rolls back to `delegation-server:latest` (the old image) and
   the job fails. Read the logs it prints, fix the problem, and rerun.

### 5. After the first successful deploy (ANTON)

```bash
cd ~/delegation
git checkout main && git pull
./server/deploy.sh status                       # ghcr.io/sterance/delegation-server:<sha>
docker image rm server-delegation-server:latest # old name for the pre-pipeline image
```

`delegation-server:latest` is still the rollback target for one deploy.
`deploy.sh` cleans up old GHCR tags by itself from then on.

## Things to know later

- **What CI updates:** only the image. Changes to
  `server/docker-compose.yml`, `deploy.sh` and `config.env` need a
  `git pull` on ANTON, then `server/deploy.sh deploy <current sha>` or
  `docker compose up -d`. This is deliberate: letting CI push compose files
  would make the restricted key equivalent to root on the host.
- **Roll back** from GitHub with **Actions → Deploy → Run workflow**,
  setting `image_tag` to an earlier SHA. On ANTON, run
  `server/deploy.sh rollback`.
- **New server:** follow §2.1 (create a **new** tunnel for it), §2.2 and
  §2.8. For several servers, the workflow would need one environment per
  host, which isn't implemented yet.
- **Unverified assumption:** the `cloudflared access ssh` token flags
  (`--service-token-id`/`--service-token-secret`) were taken from the
  cloudflared source. The laptop test in step 2 confirms them; the runner
  installs the latest cloudflared.
