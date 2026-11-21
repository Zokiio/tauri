#!/usr/bin/env python3
"""Drive an Alloy CEF 154 browser and its service worker using only loopback fixtures."""
import base64
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import signal
import subprocess
import tempfile
from threading import Thread

URL = "http://localhost:43210"
CREDENTIALS = "Basic " + base64.b64encode(b"tb6:fixture").decode("ascii")
REQUESTS = []

PAGE = """
<!doctype html><meta charset="utf-8"><title>CEF proxy auth probe</title>
<script>
console.log("PROBE page entered script");
const url = "http://localhost:43210";
async function workerFetch(worker, path) {
  const channel = new MessageChannel();
  const response = new Promise(resolve => channel.port1.onmessage = event => {
    channel.port1.close();
    resolve(event.data);
  });
  worker.postMessage(url + path, [channel.port2]);
  return response;
}
(async () => {
  await navigator.serviceWorker.register("/sw.js");
  const { active } = await navigator.serviceWorker.ready;
  console.log("PROBE worker-cold " + await workerFetch(active, "/worker-cold"));
  const response = await fetch(url + "/page-warm");
  console.log("PROBE page-warm " + response.status + " " + await response.text());
  console.log("PROBE worker-warm " + await workerFetch(active, "/worker-warm"));
  console.log("PROBE done");
})().catch(error => console.log("PROBE fatal " + error));
</script>
"""

WORKER = """
self.addEventListener("install", event => event.waitUntil(self.skipWaiting()));
self.addEventListener("message", event => event.waitUntil((async () => {
  try {
    const response = await fetch(event.data);
    event.ports[0].postMessage(response.status + " " + await response.text());
  } catch (error) {
    event.ports[0].postMessage(String(error));
  }
})()));
"""

class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body="", headers=None):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

class Direct(Fixture):
    def do_GET(self):
        print("DIRECT " + self.path, flush=True)
        if self.path == "/":
            self.reply(200, PAGE, {"Content-Type": "text/html"})
        elif self.path == "/sw.js":
            self.reply(200, WORKER, {"Content-Type": "application/javascript"})
        else:
            self.reply(404)

class Proxy(Fixture):
    def do_GET(self):
        paths = [URL + path for path in ["/worker-cold", "/page-warm", "/worker-warm"]]
        if self.path not in paths:
            print("PROXY refused unexpected URL " + self.path, flush=True)
            self.reply(403)
            return

        authenticated = self.headers.get("Proxy-Authorization") == CREDENTIALS
        status = 200 if authenticated else 407
        REQUESTS.append((self.path.removeprefix(URL), status))
        print(f"PROXY {self.path} {status}", flush=True)
        headers = {"Access-Control-Allow-Origin": "*"}
        if status == 407:
            headers["Proxy-Authenticate"] = 'Basic realm="tb6-probe"'
        self.reply(status, "fixture" if authenticated else "", headers)

@contextmanager
def serve(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_port
    finally:
        server.shutdown()
        server.server_close()
        thread.join()

def main():
    app = Path(__file__).parent.resolve() / "ProxyProbe.app" / "Contents"
    executable = app / "MacOS" / "ProxyProbe"
    helper = app / "Frameworks" / "ProxyProbe Helper.app" / "Contents" / "MacOS" / "ProxyProbe Helper"
    with serve(Direct) as direct_port, serve(Proxy) as proxy_port, tempfile.TemporaryDirectory(
        prefix="tb6-cef-auth-"
    ) as cache:
        env = {
            **os.environ,
            "TB6_PAGE": f"http://127.0.0.1:{direct_port}/",
            "TB6_PROXY_PORT": str(proxy_port),
            "TB6_HELPER": str(helper),
            "TB6_CACHE": cache,
        }
        with subprocess.Popen(
            [
                executable,
                "--use-mock-keychain",
                "--disable-chrome-login-prompt",
                "--enable-logging=stderr",
                "--disable-gpu",
                "--disable-background-networking",
                "--disable-component-update",
                "--no-first-run",
                "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.0.0.1",
            ],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True,
        ) as probe:
            try:
                output, _ = probe.communicate(timeout=12)
            except subprocess.TimeoutExpired:
                os.killpg(probe.pid, signal.SIGKILL)
                output, _ = probe.communicate()
                print(output, end="")
                raise

    print(output, end="")
    if probe.returncode:
        raise SystemExit(f"CEF probe exited {probe.returncode}")
    expected_requests = [
        ("/worker-cold", 407),
        ("/page-warm", 407),
        ("/page-warm", 200),
        ("/worker-warm", 200),
    ]
    if REQUESTS != expected_requests:
        raise SystemExit(f"Unexpected proxy trace: {REQUESTS!r}")
    expected_output = [
        "PREF applied=1 error=",
        "RESOURCE browser=0 default=1 url=" + URL + "/worker-cold",
        "PROBE worker-cold 407",
        "RESOURCE browser=1 default=1 url=" + URL + "/page-warm",
        "PROBE page-warm 200 fixture",
        "RESOURCE browser=0 default=1 url=" + URL + "/worker-warm",
        "PROBE worker-warm 200 fixture",
        "PROBE done",
        "BROWSER closed",
    ]
    if any(line not in output for line in expected_output):
        raise SystemExit("Missing a required CEF observation")
    auth = [line for line in output.splitlines() if line.startswith("AUTH ")]
    if len(auth) != 1 or not auth[0].startswith("AUTH browser=1 proxy=1 ") or not auth[0].endswith(
        "origin=" + URL + "/page-warm"
    ):
        raise SystemExit(f"Unexpected authentication callbacks: {auth!r}")
    print("PASS: cold worker gets 407, page authenticates, warm worker reuses cached proxy auth")

if __name__ == "__main__":
    main()
