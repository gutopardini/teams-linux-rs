# Better Teams Linux port

Source: https://github.com/mrowlinson/better-teams
Snapshot: `ce451cc20949c3c9afb9ccb3d4f376cde53ca1f9`.

`vendor/ost` contains the Rust protocol core used by Better Teams. Its MIT
license is retained in `vendor/ost/LICENSE`. Local changes remove the bundled
client IDs, disable the standalone CLI binary, and add `TeamsClient::from_config`
so the Linux UI supplies an in-memory session without loading CLI credentials.
TLS uses Rustls instead of the upstream native OpenSSL backend, avoiding an
OpenSSL development-package requirement on Linux.

The local `api/files.rs` addition `upload_file_data_idem_verified` compares the
selected file's SHA-256 with the exact bytes read for upload before issuing a
request. It adds no request fields and does not change the upload contract; it
protects the app's draft integrity when a source file changes after attachment.

The authentication sequence in `src/auth.rs` is adapted from
`rust/ostmac-core/src/browser_auth.rs`, `rust/ost/src/auth/oauth.rs`, and
`swift/Sources/OstMacCore/TokenRefresh.swift`. The navigation interception in
`src/auth_webview.rs` is adapted from
`swift/Sources/BetterTeamsUI/Auth/SignInWebController.swift`.

The profile-photo flow in `src/avatars.rs` follows
`swift/Sources/OstMacCore/ProfilePhotoStore.swift` (`GraphPhotoFetcher`) and
`swift/Sources/BetterTeamsUI/Shared/Avatar.swift`: ID-based Graph photo requests,
background loading, bounded workers, caching and monogram fallback. The Linux
implementation keeps its cache in memory and draws circular textures in wgpu.

Original Better Teams portions retain the license in this directory's
`LICENSE` (including its Microsoft exclusion). Original third-party notices are
retained in `UPSTREAM-NOTICE`. These portions are not relicensed by this project.

Linux uses a separate GTK/WebKit process for the authentication window, leaving
the Iced/wgpu event loop in the application process. Client IDs are supplied by
the local configuration. No upstream client ID is embedded in this port.
