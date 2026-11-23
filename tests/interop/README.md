# Native media interop

`srtcp_peer.c` is our synthetic stdio-only peer using the official libSRTP2
public API. It does not read account configuration, open devices or use network.
The master key/salt are public fixture bytes (0x11/0x22), never real credentials.

Validated with `libsrtp-2.8.0-1.fc44.x86_64`. With libSRTP2 development headers:

```sh
cc -std=c11 -Wall -Wextra -Werror $(pkg-config --cflags libsrtp2) tests/interop/srtcp_peer.c $(pkg-config --libs libsrtp2) -o /tmp/teams-srtcp-peer
TEAMS_LINUX_SRTP2_PEER=/tmp/teams-srtcp-peer cargo test --offline --manifest-path vendor/ost/Cargo.toml --lib independent_libsrtp_accepts_native_control -- --ignored --test-threads=1
```

This machine has the runtime library but no development headers. The equivalent
verified build used the official v2.8.0 `include/srtp.h` in an isolated directory:

```sh
cc -std=c11 -Wall -Wextra -Werror -I/tmp/teams-srtcp-headers tests/interop/srtcp_peer.c -Wl,-l:libsrtp2.so.1 -o /tmp/teams-srtcp-peer
```

The JSON fixture at `vendor/ost/tests/fixtures/native-media-libsrtp.json` records
header provenance/hash, key bytes and generated vectors. `protect` generates a
compound encrypted RR/SDES; `rtp` generates rollover across two SSRCs; `unprotect`
reads one bounded hex packet from stdin. Normal Rust tests compare these vectors
without requiring the native helper. The optional test also checks libSRTP
accepts the client's initial SRTCP index0 (libSRTP's own sender begins at1).

## DTLS/SRTP

`dtls_peer.c` is a separate OpenSSL DTLS1.2 server. It binds only127.0.0.1,
generates its certificate/private key in memory and pins the test client's SHA256
fingerprint. The peer exposes only synthetic test exporter bytes to its Rust
harness; it never reads account configuration or opens media devices.

With OpenSSL development headers installed:

```sh
cc -std=c11 -Wall -Wextra -Werror tests/interop/dtls_peer.c $(pkg-config --cflags --libs openssl) -o /tmp/teams-dtls-peer
TEAMS_LINUX_DTLS_PEER=/tmp/teams-dtls-peer cargo test --offline --manifest-path vendor/ost/Cargo.toml --lib independent_openssl_peer_confirms -- --ignored --test-threads=1
```
