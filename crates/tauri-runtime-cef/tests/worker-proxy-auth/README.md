# CEF proxy auth for service workers

On macOS arm64 with the CEF 154.0.34 distribution, run:

```sh
CEF_PATH=/Users/Shared/gups-cef bash build.sh
python3 -B check.py
```

The probe opens an offscreen Alloy browser with an isolated request context and
two loopback fixture listeners. It confirms that the browser answers a
`407` through CEF's auth callback and that a service worker in the same context
then reuses the cached credentials. A service worker that fetches before the
page authenticates sees `407` with no associated browser and no auth callback.
The probe uses a disposable profile and a mocked keychain.
