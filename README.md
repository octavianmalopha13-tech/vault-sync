# vault-sync

Sync a pass-manager vault between devices.

## Model

The server is format-agnostic: it stores an opaque blob and a monotonic
version counter. It has no vault-core dependency and never sees plaintext.

The client owns all vault knowledge. Push and pull move the encrypted bytes
verbatim. Merge happens client-side, where the master password is available.

## Concurrency

Optimistic. Each client remembers the last server version it saw (in a
sidecar `<file>.sync-state` file). A push includes that version; the server
accepts only if it matches the current one. Otherwise the push returns 409
and the client must pull first.

## Merge

When pull isn't enough (both devices edited since the last sync), use
`merge`. It loads the local vault, fetches and decrypts the remote blob,
walks the remote entries adding any the local vault doesn't have, and on
conflicting entries keeps local and prints the site name as a warning.
Saves the merged result and pushes it.

## Usage

    cargo run -- serve --bind 127.0.0.1:8080
    cargo run -- push vault.enc --server http://127.0.0.1:8080
    cargo run -- pull vault.enc --server http://127.0.0.1:8080
    cargo run -- merge vault.enc --server http://127.0.0.1:8080

## Not implemented

- Server persistence: state is in-memory; restart = empty.
- Auth: anyone on the port can read the (encrypted) blob or overwrite it.
- TLS: localhost only.
- Field-level merge: if A changes `user` and B changes `password` on the
  same entry, one side loses. Local-wins is the current policy.
- Vector clocks: a monotonic counter detects that there was a conflict,
  not which device caused it.

## License

MIT
