# Design: Local Operator Token

## Status

Proposed

## Context

Commit b35e8cebb742 ("api: require an opt-in before loopback bypasses
authentication") made the loopback bypass conditional on
`server.trust_loopback`, which defaults to false. The reasoning still holds: a
request arriving on loopback proves nothing, because a reverse proxy in front of
the service also reaches it from loopback, and a proxy forwards
`X-Forwarded-For` only when it is configured to.

The change left the local tooling with no way to authenticate:

- `benchmark` posts to `/api/submit` with no credentials and now receives
  403 for every entry, so `cargo run --bin benchmark -- --file
  ./benchmarks/benchmark_preexisting.json` ingests nothing.
- `sashiko-cli` submit and rerun have the same gap.
- `server.trust_loopback` is not mentioned in `Settings.toml`, so nothing points
  a developer at the switch that would restore the old behaviour.
- A fresh checkout has no `server.jwt_secret` either, so no token can be minted
  and no identity can sign in. The instance is unusable for ingest until the
  developer both invents a secret and lists themselves in `server.acl`.

On top of that, `benchmark` only logs a failed submission and then enters its
wait loop, where it spins forever reporting `Missing Patches: N` for patches
that can never arrive. The symptom the developer sees is a hang, not a 403.

## Goals

1. A developer who clones the repository, starts the server and runs the
   benchmark can ingest patches with no configuration at all.
2. No weakening of the property b35e8cebb742 established: a request that merely
   arrives from loopback must not gain ingest authority.
3. The benchmark fails fast and loudly when ingestion is refused.

## Non-goals

- Any change to bug access control. Authority over a Linux kernel bug is
  resolved per bug by `BugPrincipal`, which this mechanism never touches.
- Replacing sign-in links, the ACL, or the `Permission` model.
- Personal access tokens for remote CI, which remain future work in
  DESIGN_ROLE_BASED_ACCESS_CONTROL.md.

## Design

### The credential

At startup the server generates a 256 bit random value, hex encodes it, and
writes it to a file that only its own user can read. A client that can read that
file proves it runs as the same user on the same machine, which is a materially
stronger claim than "my packet came from 127.0.0.1", and it is a claim a
proxied internet caller cannot make.

The token lives for one server process. It is regenerated on every start, so a
file left behind by a crashed server authenticates nothing.

- Path: `Settings::local_token_path()`, the directory holding the database file
  joined with `.sashiko-local-token`. A non-filesystem `database.url` (anything
  containing `://`) falls back to the current directory. Server and clients call
  the same helper, so they agree without configuration.
- Contents: 64 hex characters and a trailing newline.
- Mode: 0600, created with `OpenOptions::mode`, truncating whatever was there.
- Randomness: 32 bytes read from `/dev/urandom`. `fastrand` is deliberately not
  used, because it is not a cryptographic generator.

### Presenting it

Clients send `Authorization: Bearer <token>`, the same header a session JWT uses.
The two are told apart by shape: the local token is 64 hex characters and a JWT
is not, so no valid JWT can be mistaken for a local token and no local token
reaches the JWT verifier as a plausible signature.

`is_authorized` gains one check, placed after the blocklist and before the
loopback bypass:

```rust
if presents_local_token(headers, state) {
    return true;
}
```

The comparison is constant time via `subtle::ConstantTimeEq`, on the SHA-256
digests of the presented and expected values so that length is not compared
either.

The token grants exactly the capability set the loopback bypass grants:
`Ingest`, `Cancel` and `Review`. It is not an identity: it has no email, so it
cannot appear in an ACL, cannot be blocklisted, and never reaches a bug route.

### Server behaviour

`AppState` holds the token in memory. `main` writes the file before the API
starts listening and logs the path at info level, never the value. Failure to
write is a warning, not a fatal error: a read-only filesystem is a legitimate
deployment, and the instance is still fully usable through sign-in links.

The file is removed on clean shutdown on a best-effort basis.

### Client behaviour

`benchmark` and `sashiko-cli` read the file when it exists and attach the header.
A missing or unreadable file is not an error at that point; the request simply
goes out unauthenticated, which still succeeds under `trust_loopback` or
`testing_mode`.

`benchmark` stops treating a rejected submission as a log line. Phase 1 collects
failures and aborts before the wait loop with the status code, the response body
and, on 403, a sentence naming the likely cause: the server is running as a
different user, or from a different directory, so its token file is not the one
the benchmark read.

## Security analysis

The bar the token sets is "can read a 0600 file owned by the server's user". A
caller who clears that bar can already read `Settings.toml`, which holds
`jwt_secret` and therefore allows minting a session token for any admin in the
ACL, and can read the database directly. The token grants strictly less than
what that access already implies, so it widens nothing.

Against the specific threat b35e8cebb742 addressed, a request forwarded by a
reverse proxy from the public internet, the token is a genuine improvement: such
a caller cannot read a local file, and so is refused whatever the topology.

Residual risks, accepted:

- Another local user with the same uid, or root, can read the file. Both can
  already read the configuration and the database.
- The path is derived from the database location, so an operator who points two
  instances at one directory shares one token between them. They also share a
  database, so they are one instance in every sense that matters.

## Alternatives considered

- **Document `trust_loopback = true`.** One line of configuration, but it
  restores exactly the property that was deliberately removed, and it is
  unsafe on the deployed topology. Rejected, and the setting was removed
  outright once the token covered every local caller: see below.
- **Have the benchmark mint a JWT from `server.jwt_secret`.** Works on a machine
  that already configures a secret and an admin, which is not zero configuration
  and does not help a fresh checkout. It also teaches a local tool to forge
  identities, so a bug in it forges a maintainer.
- **A `--token` flag.** Explicit, but it is configuration by another name and
  every developer pays it on every run.
- **Unix domain socket for local administration.** The strongest option and the
  one with the clearest boundary, but it needs a second listener, a second
  client transport, and a way to express it in the CLI. Worth revisiting if
  local-only endpoints multiply; disproportionate for restoring ingest.

## Implementation plan

Each step is a separate commit.

1. `settings`: add `local_token_path` and document `trust_loopback` in
   `Settings.toml`. Unit tests for filesystem and remote database URLs.
2. `auth`: add the token type with generation, 0600 write, read, and constant
   time comparison. Unit tests for round trip, mode, length and mismatch.
3. `api`: hold the token in `AppState` and honour it in `is_authorized`. Tests
   for accepted token, wrong token, and that it does not reach a bug route.
4. `main`: write the file at startup, log the path, remove it on shutdown.
5. `benchmark`: attach the token and abort phase 1 on any rejected submission.
6. `cli`: attach the token to submit and rerun.
7. `.gitignore`: ignore `.sashiko-local-token`.

## Testing

- `make check-pr` after each commit.
- Manual: with no configuration beyond a fresh `Settings.toml`, start the server
  and run `cargo run --bin benchmark -- --file ./benchmarks/benchmark_smoke.json`,
  and confirm the entries ingest.
- Manual: with the token file removed, confirm the benchmark aborts in phase 1
  with the 403 and the explanation rather than hanging in phase 2.

## Follow-on: the loopback distinction was removed entirely

With every local tool presenting the token, nothing was left that needed the
source address, so the address stopped being consulted at all:

- `server.trust_loopback` and its branch in `is_authorized` are gone, along with
  the `x-forwarded-for` / `x-real-ip` / `forwarded` veto that partially
  compensated for the bypass being topology-blind. `is_authorized` no longer
  takes the peer address.
- `Permission::allows_loopback_bypass` became `granted_by_local_token`. It still
  matches exhaustively, so a capability added later is unreachable by the token
  until someone decides it should be.
- The forge webhook's secretless fallback accepted anything from loopback, which
  behind a reverse proxy meant anything at all. It now requires the token, the
  webhook signature, or the explicit unsafe flag.

The caller that loses out is one on the same machine that cannot read the token
file: a different local user, or a container sharing the network namespace.
Neither should ever have been trusted, and the loopback bypass could not tell
either of them apart from the internet.

`--enable-unsafe-all-submit` stays. It is an explicit statement by an operator
rather than an inference about the network, which is the distinction this whole
change turns on.
