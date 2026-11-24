# isideload 0.5.2, patched for Loadside

Vendored from crates.io (MIT, see upstream) and used through `[patch.crates-io]`
in the workspace Cargo.toml. Changes, all marked "Loadside patch":

- `src/anisette/remote_v3/mod.rs`: the provisioning loop spun forever once the
  WebSocket closed (`else { continue }`), and waited forever on a silent
  server. It now fails on close and times out after 60 s per message. The
  anisette HTTP client gets connect (20 s) and request (60 s) timeouts, and
  only follows redirects within the same server and scheme.
- `src/auth/grandslam.rs`: Apple HTTP client gets connect (20 s) and request
  (90 s) timeouts.
- `src/sideload/cert_identity.rs`: an expired (or about to expire) certificate
  that Apple still lists is no longer reused; a fresh one is requested.
- `src/sideload/bundle.rs`: the .dylib search no longer follows symlinks
  (a link to a parent folder made it recurse without end).

Drop this copy once upstream has equivalent fixes.
