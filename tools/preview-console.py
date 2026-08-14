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
# Optional: a directory of canned replies, one file per route, named after the route with
# the slashes turned into dashes -- `api-wifi.json` answers `/api/wifi`. For looking at a
# state the appliance is not in, which is most of them: a card that only appears while
# something is failing is a card nobody ever looks at before shipping it.
CANNED = sys.argv[4] if len(sys.argv) > 4 else None

# The two POST routes that only read. Both are POSTs so that the console's method-based gate
# applies to them -- a process list with command lines, and what somebody is watching, are
# private in a way a root hash is not -- and neither changes anything on the appliance, so a
# preview may forward them. Every other POST here installs, erases or restarts.
READ_ONLY_POSTS = ("/api/metrics/processes", "/api/plex/sessions")

INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def canned_for(self, path):
        """The canned reply for a route, if one was recorded."""
        if not CANNED or not path.startswith("/api/"):
            return None
        import os.path
        name = path.strip("/").replace("/", "-") + ".json"
        candidate = os.path.join(CANNED, name)
        return candidate if os.path.exists(candidate) else None

    def do_POST(self):
        """Answers the POST routes from the canned directory, or proxies the two that read.

        Added when the activity card arrived, whose one control is a POST: the process list
        is not a GET on purpose, because every GET on this console answers without a
        credential and a list of what is running with its command lines should not. Without
        this the preview could render the button and never show what pressing it does, which
        is the same hole this whole tool exists to close.

        A POST on this console usually changes the machine -- it installs Plex, erases
        disks, restarts -- and a preview that forwarded one would do it to a real appliance
        because somebody was looking at a page. So nothing is proxied except the routes in
        READ_ONLY_POSTS, which are POSTs for *privacy* rather than because they change
        anything: a process list with command lines, and what somebody is watching. Those
        two are the ones a preview most needs to show real, because canned data proves the
        card renders and not that it renders what the appliance actually says.

        The `Authorization` header is passed through, so the page has to have a real device
        token in it -- see the note at the top about seeding sessionStorage.
        """
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""

        canned = self.canned_for(self.path)
        if not canned and self.path in READ_ONLY_POSTS:
            request = urllib.request.Request(
                f"https://{APPLIANCE}{self.path}",
                data=body or b"{}",
                method="POST",
                headers={
                    "Content-Type": "application/json",
                    "Authorization": self.headers.get("Authorization", ""),
                },
            )
            try:
                with urllib.request.urlopen(request, context=INSECURE, timeout=10) as up:
                    payload = up.read()
                self.send_response(200)
            except Exception as error:  # noqa: BLE001 - a preview, not a server
                payload = str(error).encode()
                self.send_response(502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        if not canned:
            body = (
                f"the preview answers POST {self.path} only from a canned file; it does not "
                "forward a POST to the appliance, because a POST here changes the machine\n"
            ).encode()
            self.send_response(501)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        with open(canned, "rb") as handle:
            body = handle.read()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == DELAY_PATH:
            time.sleep(SETTLE_SECONDS)
            self.send_response(200)
            self.send_header("Content-Type", "image/gif")
            self.send_header("Content-Length", str(len(PIXEL)))
            self.end_headers()
            self.wfile.write(PIXEL)
            return
        canned = None
        if CANNED and self.path.startswith("/api/"):
            import os.path
            name = self.path.strip("/").replace("/", "-") + ".json"
            candidate = os.path.join(CANNED, name)
            if os.path.exists(candidate):
                canned = candidate
        if canned:
            with open(canned, "rb") as handle:
                body = handle.read()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
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
