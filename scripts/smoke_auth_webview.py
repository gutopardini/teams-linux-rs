"""Exercise native WebKit callback interception with a local, fake login page.

Usage: python3 scripts/smoke_auth_webview.py target/release/teams-linux-rs
No Microsoft requests, local .env reads, or real credentials are involved.
"""

import http.server
import json
from pathlib import Path
import subprocess
import sys
import threading
import urllib.parse


REDIRECT = "https://login.microsoftonline.com/common/oauth2/nativeclient"
CALLBACK = REDIRECT + "?code=fixture-code&state=fixture-state"


class LoginFixture(http.server.BaseHTTPRequestHandler):
    hits = 0

    def do_GET(self):
        type(self).hits += 1
        body = ("<!doctype html><meta charset=utf-8><title>Local login fixture</title>"
                "<p>Passkey not registered. This browser does not support passkeys.</p>"
                "<script>setTimeout(() => location.replace("
                + json.dumps(CALLBACK) + "), 400)</script>").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/teams-linux-rs").resolve()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), LoginFixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        request = {
            "authorize_url": f"http://127.0.0.1:{server.server_port}/login-fixture",
            "redirect_uri": REDIRECT,
        }
        process = subprocess.run(
            [str(binary), "--auth-webview-helper"],
            input=json.dumps(request), text=True, capture_output=True, timeout=45,
        )
        if process.returncode:
            raise AssertionError(f"Helper exit {process.returncode}: {process.stderr[-1500:]}")
        reply = json.loads(process.stdout)
        assert reply["status"] == "callback", reply.get("message", reply["status"])
        query = urllib.parse.parse_qs(urllib.parse.urlparse(reply["url"]).query)
        assert query == {"code": ["fixture-code"], "state": ["fixture-state"]}
        assert LoginFixture.hits >= 1
        print("PASS: native WebKitGTK window intercepted nativeclient and returned the callback over its pipe.")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
