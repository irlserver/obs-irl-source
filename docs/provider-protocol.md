# Provider protocol

A provider is a service that hosts ingests for the plugin's users. The properties dialog signs in to it, lists the user's ingests by name, and writes the chosen pull URL into the URL field. The plugin ships no provider specific code. A service becomes a provider by publishing one JSON document and two HTTP endpoints, and by being an OAuth 2.0 authorization server.

This document is the contract, version 1. A provider implements it against a host of its own; every URL below is an example.

## Design constraints

**The plugin is dumb.** It knows nothing about regions, protocols, stream keys, sharing, or entitlements. Every such decision is made server side and surfaced as a flat list of named entries. If a choice has to be made (which region to pull from), the server either makes it or expands it into separate entries.

**No keys leave the server until the user picks one entry.** The list response carries names and opaque ids only. The pull URL, which embeds the stream key, is returned by a second call for one id at a time, and the plugin writes it straight into the OBS setting. The plugin never stores a URL in its own state file.

**Streaming never depends on sign in.** The receiver reads only the URL setting. An expired session, an unreachable provider, or a signed out plugin only stops the dropdown from filling. A scene collection saved months ago keeps working. `crates/irl-source/tests/provider_seam.rs` pins this.

**The plugin identifies itself.** Every request carries `User-Agent: obs-irl-source/<version>`. The provider decides who gets to use it through the `client_id` it issues and the `min_plugin_version` it publishes.

## What the user sees

The dialog starts with a Provider dropdown. The stock build lists the built in providers plus a Custom entry, which reveals a text field for a provider base URL. Below it sit the ingest list and the sign in, refresh and sign out buttons for the selected provider, then the plain URL field. Picking an ingest writes its URL into that field and resets the ingest list to its blank entry, so the pick is an action, not a second source of truth.

The built in list and the Custom entry are constants in `irl-core`. A fork that ships a build for one provider pins the list to itself and drops Custom; nothing else changes.

The provider choice and the picked id are OBS settings, so they land in the scene collection. That is why the contract forbids secrets in ids.

## Discovery

The plugin fetches, without credentials:

```
GET {base}/.well-known/irl-source-provider.json
```

```json
{
	"protocol_version": 1,
	"id": "example",
	"name": "Example Relays",
	"issuer": "https://auth.provider.example",
	"client_id": "obs-irl-source",
	"scope": "openid",
	"ingests_endpoint": "https://api.provider.example/irl-source/ingests",
	"min_plugin_version": "2.1.0"
}
```

- `protocol_version`: the plugin refuses a version it does not know.
- `id`: stable slug, `^[a-z0-9-]{1,32}$`. The plugin names the provider's state file after it and prefixes the ids it stores in OBS settings with it. Changing it signs every user out.
- `name`: the label of this provider in the Provider dropdown.
- `issuer`: the OAuth 2.0 issuer. The plugin loads `{issuer}/.well-known/openid-configuration` and reads `authorization_endpoint`, `token_endpoint`, `registration_endpoint` and `revocation_endpoint` from it. Nothing else in that document is used.
- `client_id`: optional. A pre-registered public client the plugin uses as is. When absent, the plugin registers itself through `registration_endpoint` (RFC 7591) on first sign in and caches the returned id. Pre-registering is preferred: it gives the provider one row to allowlist or revoke.
- `scope`: the scope string the plugin requests, verbatim.
- `ingests_endpoint`: absolute URL of the list endpoint. The resolve endpoint is derived from it.
- `min_plugin_version`: optional semver. An older plugin shows a message instead of a sign in button.

The document must be served as `application/json`. CORS does not matter; the plugin is not a browser.

## Sign in

OAuth 2.0 authorization code with PKCE (`S256`), public client (`token_endpoint_auth_method: none`), loopback redirect per RFC 8252 section 7.3.

1. The plugin binds `127.0.0.1` on the first free port from `47420` to `47429`.
2. It opens the system browser at `authorization_endpoint` with `response_type=code`, `client_id`, `redirect_uri=http://127.0.0.1:{port}/callback`, `scope`, `state` (a 128 bit nonce), `code_challenge` and `code_challenge_method=S256`.
3. The browser lands on `redirect_uri` with `code` and `state`. The plugin answers with a small "you can close this tab" page and stops listening.
4. The plugin exchanges the code at `token_endpoint` with `grant_type=authorization_code`, `code_verifier`, `redirect_uri` and `client_id`.
5. It stores the refresh token in the plugin's config directory (mode 0600 on macOS and Linux), never in the scene collection. The access token stays in memory.

A sign in the user does not finish times out after two minutes and the port is released.

The authorization server must accept all ten loopback redirect URIs for the client. RFC 8252 asks servers to accept any port on a loopback redirect, but many match registered URIs exactly, so the client is registered with all ten. A dynamically registered client sends all ten in `redirect_uris`.

Refresh: on a 401 from either endpoint below, the plugin calls `token_endpoint` with `grant_type=refresh_token` once and retries. If the refresh fails, the plugin signs out and the dropdown empties.

Sign out: the plugin posts the refresh token to `revocation_endpoint` (RFC 7009), ignores the result, and deletes its state file. Revocation is best effort, so the session disappears from the user's active sessions instead of lingering to expiry.

## List ingests

```
GET {ingests_endpoint}
Authorization: Bearer {access_token}
Accept: application/json
```

```json
{
	"ingests": [
		{ "id": "a1b2:eu", "name": "Main phone", "detail": "Europe", "online": true, "bitrate_kbps": 4200 },
		{ "id": "a1b2:us", "name": "Main phone", "detail": "US East", "online": true, "bitrate_kbps": 4200 },
		{ "id": "c3d4:eu", "name": "Backup phone", "detail": "Europe", "online": false }
	]
}
```

- `id`: opaque to the plugin, `^[A-Za-z0-9._:-]{1,128}$`, stable across calls for the same entry. It is what the dropdown stores, so it must not contain a secret.
- `name`: required. Primary text.
- `detail`: optional secondary text. Region, owner of a shared ingest, anything the server wants the user to see.
- `online`, `bitrate_kbps`: optional. Absent means unknown and the plugin shows nothing for it. Serve status from a cache; the plugin gives the call five seconds.
- Order is display order. Put the likely pick first.

One entry per thing the user can pull. If an ingest is reachable from several regions, the server returns one entry per region and says which in `detail`. The plugin does not group, sort, or dedupe.

Responses other than 200 use the OAuth error shape: `{ "error": "…", "error_description": "…" }`. 401 triggers the refresh above. Anything else is logged and the cached list stays.

## Resolve one ingest

```
GET {ingests_endpoint}/{id}/url
Authorization: Bearer {access_token}
```

```json
{ "url": "srt://relay.provider.example:4000?streamid=play/stream/abc123" }
```

The plugin writes `url` into its URL setting and forgets it. The server may mint, rotate, or scope the key inside the URL however it likes; nothing in the plugin depends on the URL's shape beyond FFmpeg being able to open it.

- 404: the id is unknown or the user no longer has access to it. The plugin leaves the URL setting alone and refreshes the list.
- 403: the user may see this ingest but not pull it. Same handling as 404, with `error_description` in the OBS log.

## Timeouts and identification

Every request has a 3 second connect timeout and a 5 second total timeout, and carries `User-Agent: obs-irl-source/<version>`.

## Requirements on the authorization server

OIDC discovery, authorization code grant with PKCE, refresh tokens, public clients, and either RFC 7591 registration or a pre-registered `client_id`. A custom redirect that hands a session token to the loopback listener directly is not supported; the plugin speaks OAuth only.
