# VERTEXIA deployment and networking

Copyright (C) 2026 ItsWempo. SPDX-License-Identifier: AGPL-3.0-or-later.

RaveEngine is the native client and game-server runtime for the separate VERTEXIA platform. The platform's web/API origin is `https://vertexia.wempos-forum.com`.

## Service boundaries

| Service | Responsibility | Transport |
| --- | --- | --- |
| VERTEXIA web/API | Accounts, Studio authorization, game discovery, join tickets, asset APIs | HTTPS |
| RaveEngine client | Native Bevy game client | HTTPS for platform APIs; UDP for gameplay |
| RaveEngine game server | Authoritative simulation, player replication, Netcode handshake | UDP |
| Cloudflare Worker + Container | Public website/API ingress and existing Go application | HTTP/HTTPS and WebSockets |

The Cloudflare Worker is not the game-server socket. Do not point a Lightyear UDP client at the HTTP Worker hostname.

## HTTP API configuration

The default API origin is `https://vertexia.wempos-forum.com`.

Set `VERTEXIA_API_DOMAIN` to override it for local development or another deployment. The legacy `VERTIGO_API_DOMAIN` is still accepted if the new variable is unset. The new variable wins if both are set.

The existing API calls include:

- `GET /api/v1/auth/validate?ukey=...` with `X-Gameserver-Key` from a game server.
- `GET /api/v1/studio/poll?state=...` and `POST /api/v1/studio/exchange` for Studio authorization.
- `POST /api/v1/games/:id/join` and related game-server endpoints for game joins and lifecycle operations.
- Asset and avatar endpoints used by the game server.

The platform's API origin is not a game-server address.

## Multiplayer UDP networking

The current client and server use Lightyear 0.28 with UDP transport and Netcode. The game protocol sends unreliable sequenced input at 60 Hz and reliable ordered game messages. The server must be directly reachable by the client over UDP.

The join URI already supports a hostname in its `ip` parameter (or `host` alias):

```text
vertexia-client://play?ip=games.vertexia.example&port=5100&ukey=<short-lived-user-key>
```

The host and port must identify the actual game-server node and its UDP listener. The platform should return this endpoint in the join response. Use a separate DNS record such as `games.vertexia.wempos-forum.com` for game traffic; it must resolve to the game-server host, not the Worker.

For a self-hosted game-server node:

- Bind the server to `0.0.0.0` (the process launcher already passes this when configured with its default loopback bind).
- Allow the configured game-server UDP port range through the host firewall and provider firewall.
- Set `GSM_PUBLIC_HOST` to the public DNS hostname or address that clients can reach.
- Set `GSM_PORT_MIN` and `GSM_PORT_MAX` to the UDP port range opened on that host.
- Keep `GAMESERVER_API_KEY` on the server host only. It authenticates server-to-platform HTTP requests and must never be placed in a client build.
- Set `VERTEXIA_API_DOMAIN=https://vertexia.wempos-forum.com` in the server process environment so authentication and asset requests reach the platform.

A game-server node needs a real public UDP-capable host. Cloudflare Containers route end-user requests through a Worker and do not expose arbitrary inbound UDP to a Container. Cloudflare Spectrum can proxy custom UDP applications, but Cloudflare's current plan documentation lists custom UDP as an Enterprise add-on. Do not assume the regular Workers Paid plan or a proxied DNS record provides UDP game transport.

References:
- [Cloudflare Containers lifecycle and networking](https://developers.cloudflare.com/containers/platform-details/architecture/)
- [Cloudflare Spectrum protocols per plan](https://developers.cloudflare.com/spectrum/protocols-per-plan/)
- [Cloudflare Spectrum configuration](https://developers.cloudflare.com/spectrum/reference/configuration-options/)

## Netcode key handling

The current client startup accepts `NETCODE_PRIVATE_KEY` (or `--netcode-key`) and a protocol ID, while the server uses the matching Netcode configuration. A value embedded in, passed to, or otherwise distributed with a client is extractable. Treat the current shared Netcode key as public once a client build is distributed; do not reuse it as a platform, database, or gameserver API secret. Before broad public release, replace a long-lived shared client/server key with a design using short-lived, server-scoped connection credentials and rotation.

The `GAMESERVER_API_KEY` is different: it is server-only and must not be shipped in the client.

## Deployment relationship

The VERTEXIA website/API and RaveEngine are separate repositories and deployables. Deploying the Go application to the Cloudflare Worker does not deploy a UDP game server or make game ports reachable. The website deployment's `GSM_ENABLED=false` setting is intentional until a separate game-server host and manager integration are configured.

Do not enable game-server creation on the web Container until the manager can safely create and supervise processes on the dedicated game-server host, persist server state, and return reachable host/port values to the join flow.
