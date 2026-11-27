![RaveEngine](repo/rave.png)

# RaveEngine-Game

Welcome to the RaveEngine project, a full rewrite of the previous client built in Godot to transition to Rust. RaveEngine is an engine built with Bevy, a code-only game engine that allows us to skip the boring low-level part and get straight to what's important: the studio, the client and the server.

With Rust, we are able to build things we previously considered impossible or too hard with Godot. Many Godot features limited us for what we actually wanted with VERTEXIA. So, with the VERTIGO project (a rewrite of the website to Go), we thought, "why not rewrite the client too?". After all, it was getting hard to move around the client source code: it was as much of a mess as you think it was.

## Building

Building and running RaveEngine is fairly easy.

Head to the Rust website to install Rust and its tools:

https://rust-lang.org/tools/install/

Clone our repo, run `cargo build` (`cargo build` builds as debug, which is faster but may run worse and leave a bigger footprint in the disk; compile with the flag `--release` for a smaller file but longer compilation time), and **copy the `assets` folder you'll find the root of the project and paste it in `/target/[...]/`**. THIS IS A VERY IMPORTANT STEP!

You can now launch the executable!

## Playing through VERTEXIA

The client and Studio talk to the VERTEXIA platform at `https://vertexia.wempos-forum.com`. Set `VERTEXIA_API_URL` to use another one (`VERTIGO_API_DOMAIN` still works but is deprecated). It must be an `https://` origin; plain `http://` is only accepted for `localhost`/loopback. An invalid value is logged and ignored.

**Logging in.** The client and Studio open the platform login in your browser. The resulting access token is stored in your user config directory, readable only by you (0600):

- Linux: `$XDG_CONFIG_HOME/vertexia/` or `~/.config/vertexia/`
- macOS: `~/Library/Application Support/VERTEXIA/`
- Windows: `%APPDATA%\VERTEXIA\`

The client uses `client_auth.json` and Studio uses `studio_auth.json`. Logging out deletes the file and revokes the token. Older Studio credentials next to the executable are moved there on first start.

**Joining a game.** Start the client with no arguments to browse games, or with `--game <id>` (or a `vertexia-client://play?game=<id>` link) to join one directly. The client asks the platform for a connect token and connects to the game server it was issued for. It never needs the server's netcode key.

Developers can still connect straight to a local server with `--dev-legacy-connect --ip <ip> --port <port> --ukey <ukey>` (or `VERTEXIA_DEV_LEGACY_CONNECT=1`). This builds the token locally from `--netcode-key`/`NETCODE_PRIVATE_KEY` and only works against a server started with `--dev --legacy-ukey-auth`.

## Running a dedicated server

Servers run in production mode unless started with `--dev` (or `VERTEXIA_DEV=1`). In production:

| Variable | Purpose |
| --- | --- |
| `NETCODE_PRIVATE_KEY` | Required. 64 hex characters, not all zeros. Shared with the platform, which seals connect tokens with it. Never pass it on the command line. |
| `NETCODE_PROTOCOL_ID` | Required. Unsigned 64-bit protocol id; must match the platform's. |
| `GAMESERVER_API_KEY` | Lets the server report joins, leaves and heartbeats to the platform and fetch assets. Sent only as the `X-Gameserver-Key` header. |
| `VERTEXIA_SERVER_ID` | The platform's id for this server (`--server-id`, or `VERTIGO_SERVER_ID` from the launcher). Tokens issued for other servers are rejected. |
| `VERTEXIA_GAME_ID` | Optional game id (`--game-id`, or `VERTIGO_GAME_ID`). |

Players are admitted by the identity sealed in their connect token. The old "hello with ukey" login is off by default; enable it with `--legacy-ukey-auth` (`VERTEXIA_LEGACY_UKEY_AUTH=1`). `--allow-unauthenticated` requires `--dev`.

Once the UDP socket is listening the server prints `VERTEXIA_SERVER_READY bind=<ip>:<port> server_id=<id>`. On SIGTERM or Ctrl+C it kicks everyone with a shutdown message, reports their leaves, waits up to 5 seconds for the reports to go out and exits 0. A second signal exits immediately.

### Connect token format

The platform must mint tokens exactly as lightyear_netcode 0.28 reads them. Unlike the netcode.io reference, **every integer is big-endian**. The 256-byte user data carries the VERTEXIA identity:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | user id, u64 little-endian; must equal the token's client id |
| 8 | 32 | server id, ASCII, NUL padded |
| 40 | 1 | username length N (0 = none) |
| 41 | N | username, UTF-8, at most 64 bytes |

`tests/fixtures/connect_token_vector.json` is a complete worked example (key, nonce, plaintext and the resulting token) for checking a platform implementation. `src/common/net/connect_token.rs` documents the full byte layout.

## AI assistance disclosure

If I were to rate this on a scale of 1-5 where "1" is the least vibecoded and "5" is the most vibecoded, I'd say 2/2.5. I mostly used it for tests and debugging, as well as some visual things I couldn't figure. This sped up development considerably, and helped me find numerous amount of bottlenecks and bugs in my code. 

Though, our stance on AI is clear: all forms of AI are not allowed in our platform: one thing is using it as a tool to help and speed up development, the other is to steal a writer's job, or an artist, or a photographer.

