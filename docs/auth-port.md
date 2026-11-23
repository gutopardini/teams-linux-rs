# Work-account authentication

The implementation is ported from Better Teams snapshot
`ce451cc20949c3c9afb9ccb3d4f376cde53ca1f9`.

## Runtime flow

1. `AuthConfig::work` receives the locally configured client ID.
2. A helper process opens a native GTK/WebKitGTK authentication window.
3. Authorization uses tenant `common`, PKCE S256, `state`, the
   `nativeclient` redirect, and the Teams resource's `.default` scope with
   `offline_access`, matching Better Teams' work-account configuration.
4. The helper intercepts the exact redirect origin/path before loading it and
   returns its callback over a private pipe. The parent validates `state` and
   exchanges the code with the original PKCE verifier.
5. The Teams access token is exchanged for a Skype session token and regional
   endpoints. Graph receives a separate token via the refresh grant and Graph's
   `.default` scope. Tokens for different audiences are never interchangeable.
6. Chats, messages, and sends use the vendored `ost` native chat API. Team and
   channel directories use the Graph token. Derived-token errors belong to their
   respective service rather than being mistaken for a failed browser callback.

The existing device-code screen is the fallback, including when selected from
the authentication window. It bootstraps the same Teams resource.

## Configuration

The work-account client ID is the public Teams application
`1fec8e78-bce4-4aaf-ab1b-5451cc387264`, fixed in the application. The saved
session stays valid for that same id.

The selected application must be authorized by the identity provider for its
requested resource and redirect. A denied grant remains an authentication
error; this port does not fabricate consent or change tenant device policies.

## Linux host

The login helper loads the GTK3, GLib, GObject, and WebKitGTK 4.1 runtime libraries
already installed on the machine. It does not require their development headers.
The helper owns its GTK event loop; Iced/wgpu continues rendering the application.
Only authentication uses WebKit. Application chats are rendered by Iced.

Authorization codes travel through pipes, not process arguments or logs. The
helper uses an ephemeral browser context, exits on parent termination, and offers
retry, cancellation, and the device-code fallback. Refresh tokens are saved in
the Linux Secret Service after authentication and restored on startup. A session
from an earlier build that kept tokens only in memory requires one new sign-in.

As in Better Teams' `SignInPageCheck`, the window checks visible page text for
steps that need the system browser, including unsupported passkeys and device
policy errors. The text is used only in memory for classification; input values
are not read and page text is not logged or sent over the helper pipe. The footer
keeps the browser-fallback instruction visible after the page finishes loading.

## Validation

Unit fixtures check the upstream OAuth parameters, RFC 7636 PKCE vector, exact
callback matching, state/duplicate rejection, separate Graph/Skype tokens,
refresh-token rotation, and the shared device-code resource. A native WebKit
smoke fixture captures a fake nativeclient callback without Microsoft access.
