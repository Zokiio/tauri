---
"tauri-runtime-cef": minor:feat
"tauri-bundler": minor:feat
---

Added `Cef::frame_hosts_without_ipc`: frames whose document URL is on one of the given hosts, and every frame nested in them, get no `window.ipc`, reach no custom scheme (Tauri's `ipc` protocol included) but those hosts, only navigate to those hosts, and open no popup. With any host listed, frames whose parent is in another renderer process get no `window.ipc` either, and the browser process drops IPC messages from frames on or nested in those hosts. An application can embed untrusted documents (served from a custom scheme, for instance) without exposing the IPC channel to them. The macOS `tauri-cef-helper` the bundler compiles reads the same list.
