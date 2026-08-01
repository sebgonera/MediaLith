"""Serve the console page from the working tree, with /api/* proxied to a live appliance.

Three page changes have reached a machine broken, and every one of them passed the tests:
a duplicate `const`, a section whose markup was never added, and two elements sharing an
id. The tests assert that strings appear in the page, which all three satisfy. Nothing in
this repository had ever *rendered* the page, and that is the gap this closes -- run it,
point a headless browser at it, and look.

    python3 tools/preview-console.py crates/plexosd/src/ui/console.html 192.168.2.102 8791
    firefox --headless --profile /tmp/p --window-size=1500,2400 \\
            --screenshot ~/console.png http://127.0.0.1:8791/

Two things it does, both of which had to be learnt the hard way:

*   It serves over **plain HTTP on localhost**. The appliance's certificate is one it
    issued itself, and a headless browser will not accept it without a profile to trust
    it in -- so proxying is less work than teaching a throwaway profile about a key.
*   It makes the page **settle before `load` fires**. A screenshot is taken on `load`,
    and every section of this page arrives after that, so a shot with no delay catches
    five cards saying "Loading..." and proves nothing. `load` does wait for images, so
    the page is given one that answers slowly.

Note for the snap Firefox on the build host: a snap cannot see dot-directories in $HOME,
so the profile and the screenshot must go somewhere that does not start with a dot. The
symptom is "Firefox is already running, but is not responding", which is about neither.
"""

import http.server
import socketserver
import ssl
import sys
import time
import urllib.request

# A headless screenshot is taken on `load`, and every section of this page arrives after
# that -- so a shot with no delay catches five cards saying "Loading..." and is worth
# nothing. `load` does wait for images, so the page gets one that answers slowly.
SETTLE_SECONDS = 6
DELAY_PATH = "/__settle.gif"
PIXEL = bytes.fromhex("47494638396101000100800000000000ffffff21f90401000000002c00000000010001000002024401003b")

PAGE = sys.argv[1]
APPLIANCE = sys.argv[2]
PORT = int(sys.argv[3])

INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == DELAY_PATH:
            time.sleep(SETTLE_SECONDS)
            self.send_response(200)
            self.send_header("Content-Type", "image/gif")
            self.send_header("Content-Length", str(len(PIXEL)))
            self.end_headers()
            self.wfile.write(PIXEL)
            return
        if self.path.startswith("/api/") or self.path == "/healthz":
            try:
                with urllib.request.urlopen(
                    f"https://{APPLIANCE}{self.path}", context=INSECURE, timeout=10
                ) as upstream:
                    body = upstream.read()
                    kind = upstream.headers.get("Content-Type", "application/json")
                self.send_response(200)
            except Exception as error:  # noqa: BLE001 - a preview, not a server
                body = str(error).encode()
                kind = "text/plain"
                self.send_response(502)
        else:
            with open(PAGE, "rb") as handle:
                body = handle.read().replace(
                    b"</body>",
                    b'<img src="' + DELAY_PATH.encode() + b'" alt="" style="display:none">'
                    b"</body>",
                )
            kind = "text/html; charset=utf-8"
            self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """Threaded, or the page's six parallel fetches queue and the screenshot
    catches half of them still saying "Loading...".."""

    daemon_threads = True


Server(("127.0.0.1", PORT), Handler).serve_forever()
