// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, OnceLock};

use std::collections::HashMap;

use cef::*;
use tauri_runtime::{UserEvent, webview::DetachedWebview};

use crate::{
  cef_impl::client::TauriCefBrowserClient, runtime::CefRuntime, webview::CefWebviewDispatcher,
};

const IPC_MESSAGE_NAME: &str = "tauri:ipc";
const IPC_POST_MESSAGE_FUNCTION: &str = "postMessage";

/// Switch that passes [`crate::Cef::frame_hosts_without_ipc`] to renderer processes, as a
/// comma-separated list of hosts.
///
/// The renderer never sees the `Cef` builder, so the browser process appends it in
/// `BrowserProcessHandler::on_before_child_process_launch`.
///
/// On macOS, renderers run the `tauri-cef-helper` binary the bundler compiles
/// (`crates/tauri-bundler/cef-helper/main.rs`), which parses this switch and skips the
/// same frames itself. Keep both in sync.
pub(crate) const FRAME_HOSTS_WITHOUT_IPC_SWITCH: &str = "tauri-cef-frame-hosts-without-ipc";

/// The hosts the browser process passed in [`FRAME_HOSTS_WITHOUT_IPC_SWITCH`], lowercased.
///
/// Empty if the switch is absent. The last occurrence wins; empty entries are dropped.
pub(crate) fn frame_hosts_without_ipc_from<I>(args: I) -> Vec<String>
where
  I: IntoIterator<Item = std::ffi::OsString>,
{
  let prefix = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=");
  let mut hosts = Vec::new();
  // Non-Unicode arguments are skipped; `std::env::args` would panic on them.
  for arg in args {
    if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix(&prefix)) {
      hosts = value
        .split(',')
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect();
    }
  }
  hosts
}

fn frame_hosts_without_ipc() -> &'static [String] {
  static HOSTS: OnceLock<Vec<String>> = OnceLock::new();
  HOSTS.get_or_init(|| frame_hosts_without_ipc_from(std::env::args_os()))
}

/// The host of `url`, without port or brackets; for a `blob:` URL, the host of its origin.
///
/// `None` for URLs without an authority, such as `about:blank`, `about:srcdoc` or `data:`.
pub(crate) fn url_host(url: &str) -> Option<&str> {
  let url = url.strip_prefix("blob:").unwrap_or(url);
  let (_scheme, rest) = url.split_once("://")?;
  let authority = rest.split(['/', '?', '#']).next()?;
  let host = authority
    .rsplit_once('@')
    .map_or(authority, |(_, host)| host);
  let host = match host.strip_prefix('[') {
    Some(bracketed) => bracketed.split(']').next()?,
    None => host.split(':').next()?,
  };
  (!host.is_empty()).then_some(host)
}

/// Whether `url` is on one of `hosts`.
pub(crate) fn url_on_hosts(url: &str, hosts: &[String]) -> bool {
  url_host(url).is_some_and(|host| hosts.iter().any(|h| h.eq_ignore_ascii_case(host)))
}

/// Whether a navigation from `current` to `target` takes a frame off `hosts`, which is refused:
/// see [`crate::Cef::frame_hosts_without_ipc`].
pub(crate) fn navigation_leaves_hosts(current: &str, target: &str, hosts: &[String]) -> bool {
  url_on_hosts(current, hosts) && !url_on_hosts(target, hosts)
}

/// A frame and the frames it is nested in, as [`frame_without_ipc`] walks them.
pub(crate) trait FrameTree: Clone {
  /// The frame's document URL.
  fn document_url(&self) -> String;
  /// Whether this is the top frame of its browser, a popup's included.
  fn is_main_frame(&self) -> bool;
  /// The frame it is nested in, if this process can see it.
  fn parent_frame(&self) -> Option<Self>;
}

impl FrameTree for Frame {
  fn document_url(&self) -> String {
    CefString::from(&self.url()).to_string()
  }

  fn is_main_frame(&self) -> bool {
    self.is_main() != 0
  }

  fn parent_frame(&self) -> Option<Self> {
    self.parent()
  }
}

/// Whether the frame or any frame it is nested in is on one of `hosts`, or may be.
///
/// Nested frames count because a document can script the `about:blank` and `about:srcdoc`
/// frames it creates, which have no host of their own. A frame whose parent is in another
/// renderer process counts too: a renderer sees only the frames in its own process, and a
/// frame on another site (one an app embeds, say) is in another process than the frame on
/// `hosts` above it. The browser process sees every frame, so there the walk is exact.
pub(crate) fn frame_without_ipc<F: FrameTree>(frame: &F, hosts: &[String]) -> bool {
  let mut frame = Some(frame.clone());
  while let Some(current) = frame {
    if url_on_hosts(&current.document_url(), hosts) {
      return true;
    }
    frame = current.parent_frame();
    if frame.is_none() && !current.is_main_frame() {
      return true;
    }
  }
  false
}

pub(crate) type IpcHandler<T> =
  dyn Fn(DetachedWebview<T, CefRuntime<T>>, http::Request<String>) + Send;

wrap_v8_handler! {
  struct IpcPostMessageV8Handler;

  impl V8Handler {
    fn execute(
      &self,
      name: Option<&CefString>,
      _object: Option<&mut V8Value>,
      arguments: Option<&[Option<V8Value>]>,
      retval: Option<&mut Option<V8Value>>,
      exception: Option<&mut CefString>,
    ) -> std::os::raw::c_int {
      let Some(name) = name else {
        return 0;
      };
      if name.to_string() != IPC_POST_MESSAGE_FUNCTION {
        return 0;
      }

      let Some(message) = arguments
        .filter(|arguments| arguments.len() == 1)
        .and_then(|arguments| arguments[0].as_ref())
        .filter(|argument| argument.is_string() != 0)
      else {
        if let Some(exception) = exception {
          *exception = CefString::from("window.ipc.postMessage expects a string argument");
        }
        return 1;
      };

      let Some(context) = v8_context_get_current_context() else {
        return 1;
      };
      let Some(frame) = context.frame() else {
        return 1;
      };

      let body = CefString::from(&message.string_value()).to_string();
      let url = CefString::from(&frame.url()).to_string();
      let mut process_message = process_message_create(Some(&CefString::from(IPC_MESSAGE_NAME)));
      if let Some(args) = process_message
        .as_ref()
        .and_then(ProcessMessage::argument_list)
      {
        args.set_string(0, Some(&CefString::from(url.as_str())));
        args.set_string(1, Some(&CefString::from(body.as_str())));
        frame.send_process_message(ProcessId::BROWSER, process_message.as_mut());
      }

      if let Some(retval) = retval {
        *retval = v8_value_create_undefined();
      }
      1
    }
  }
}

fn install_ipc_post_message(context: Option<&mut V8Context>) {
  let Some(window) = context.and_then(|context| context.global()) else {
    return;
  };
  let attributes = sys::cef_v8_propertyattribute_t(
    [
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTENUM,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE,
    ]
    .into_iter()
    .fold(0, |acc, attr| acc | attr.0),
  )
  .into();
  let Some(mut ipc) = v8_value_create_object(None, None) else {
    return;
  };
  let mut handler = IpcPostMessageV8Handler::new();
  let post_message_name = CefString::from(IPC_POST_MESSAGE_FUNCTION);
  let Some(mut post_message) =
    v8_value_create_function(Some(&post_message_name), Some(&mut handler))
  else {
    return;
  };
  ipc.set_value_bykey(
    Some(&post_message_name),
    Some(&mut post_message),
    attributes,
  );
  window.set_value_bykey(Some(&CefString::from("ipc")), Some(&mut ipc), attributes);
}

// The standalone macOS helper mirrors this key and browser tracking.
const WITHOUT_IPC_KEY: &str = "tauri_without_ipc";

#[derive(Default)]
struct IpcDisabledBrowsers(HashMap<i32, usize>);

impl IpcDisabledBrowsers {
  fn created(&mut self, id: i32, extra_info: Option<&DictionaryValue>) {
    let key = CefString::from(WITHOUT_IPC_KEY);
    let marked = extra_info.is_some_and(|info| info.bool(Some(&key)) != 0);
    // During a cross-origin transition, a replacement with the same ID can
    // arrive before CEF destroys the old renderer-side browser.
    if self.0.contains_key(&id) || marked {
      *self.0.entry(id).or_default() += 1;
    }
  }

  fn destroyed(&mut self, id: i32) {
    if let std::collections::hash_map::Entry::Occupied(mut entry) = self.0.entry(id) {
      *entry.get_mut() -= 1;
      if *entry.get() == 0 {
        entry.remove();
      }
    }
  }

  fn contains(&self, id: i32) -> bool {
    self.0.contains_key(&id)
  }
}

fn ipc_disabled_browsers() -> &'static Mutex<IpcDisabledBrowsers> {
  static BROWSERS: OnceLock<Mutex<IpcDisabledBrowsers>> = OnceLock::new();
  BROWSERS.get_or_init(Mutex::default)
}

/// Marks a new browser or popup without mutating any read-only CEF metadata.
pub(crate) fn mark_without_ipc(slot: &mut Option<DictionaryValue>) -> bool {
  let Some(info) = slot
    .as_ref()
    .map_or_else(dictionary_value_create, |info| info.copy(0))
  else {
    return false;
  };
  if info.set_bool(Some(&CefString::from(WITHOUT_IPC_KEY)), 1) == 0 {
    return false;
  }
  *slot = Some(info);
  true
}

wrap_render_process_handler! {
  pub struct TauriRenderProcessHandler;

  impl RenderProcessHandler {
    fn on_browser_created(
      &self,
      browser: Option<&mut Browser>,
      extra_info: Option<&mut DictionaryValue>,
    ) {
      if let Some(browser) = browser {
        ipc_disabled_browsers().lock().unwrap().created(browser.identifier(), extra_info.as_deref());
      }
    }

    fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
      if let Some(browser) = browser {
        ipc_disabled_browsers().lock().unwrap().destroyed(browser.identifier());
      }
    }

    fn on_context_created(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      context: Option<&mut V8Context>,
    ) {
      if browser.is_none_or(|browser| ipc_disabled_browsers().lock().unwrap().contains(browser.identifier())) {
        return;
      }
      let hosts = frame_hosts_without_ipc();
      if !hosts.is_empty() && frame.is_some_and(|frame| frame_without_ipc(frame, hosts)) {
        return;
      }
      install_ipc_post_message(context);
    }
  }
}

pub(crate) fn on_process_message_received<T: UserEvent>(
  client: &TauriCefBrowserClient<T>,
  frame: Option<&mut Frame>,
  source_process: ProcessId,
  message: Option<&mut ProcessMessage>,
) -> std::os::raw::c_int {
  if source_process != ProcessId::RENDERER {
    return 0;
  }
  let Some(message) = message else {
    return 0;
  };
  if CefString::from(&message.name()).to_string() != IPC_MESSAGE_NAME {
    return 0;
  }
  // Whatever its renderer defined, a message from a frame left without `window.ipc`, or
  // nested in one, goes nowhere. The browser process sees every frame.
  let hosts = &client.context.frame_hosts_without_ipc;
  if !hosts.is_empty()
    && frame
      .as_deref()
      .is_some_and(|frame| frame_without_ipc(frame, hosts))
  {
    log::debug!("dropped an IPC message from a frame without `window.ipc`");
    return 1;
  }
  let Some(handler) = client.handlers.ipc_handler.as_ref() else {
    return 1;
  };
  let Some(args) = message.argument_list() else {
    return 1;
  };

  let mut url = CefString::from(&args.string(0)).to_string();
  if url.is_empty()
    && let Some(frame) = frame
  {
    url = CefString::from(&frame.url()).to_string();
  }
  let body = CefString::from(&args.string(1)).to_string();

  if let Ok(request) = http::Request::builder().uri(url).body(body) {
    handler(
      DetachedWebview {
        label: client.label.clone(),
        dispatcher: CefWebviewDispatcher {
          window_id: Arc::new(Mutex::new(client.window_id)),
          webview_id: client.webview_id,
          context: client.context.clone(),
        },
      },
      request,
    );
  }
  1
}

#[cfg(test)]
mod tests {
  use super::{
    FRAME_HOSTS_WITHOUT_IPC_SWITCH, FrameTree, frame_hosts_without_ipc_from, frame_without_ipc,
    navigation_leaves_hosts, url_host, url_on_hosts,
  };

  /// A frame of a renderer's view: a URL, and the chain of frames above it this process sees.
  #[derive(Clone)]
  struct Seen {
    url: &'static str,
    main: bool,
    parent: Option<Box<Seen>>,
  }

  impl FrameTree for Seen {
    fn document_url(&self) -> String {
      self.url.to_string()
    }

    fn is_main_frame(&self) -> bool {
      self.main
    }

    fn parent_frame(&self) -> Option<Self> {
      self.parent.as_deref().cloned()
    }
  }

  fn top(url: &'static str) -> Seen {
    Seen {
      url,
      main: true,
      parent: None,
    }
  }

  fn nested(url: &'static str, parent: Seen) -> Seen {
    Seen {
      url,
      main: false,
      parent: Some(Box::new(parent)),
    }
  }

  /// A frame whose parent is in another process: this process sees no frame above it.
  fn out_of_process(url: &'static str) -> Seen {
    Seen {
      url,
      main: false,
      parent: None,
    }
  }

  fn hosts(args: &[&str]) -> Vec<String> {
    frame_hosts_without_ipc_from(args.iter().map(std::ffi::OsString::from))
  }

  #[test]
  fn reads_the_hosts_the_browser_process_passed() {
    let switch = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=Apps.localhost, other.localhost,,");
    assert_eq!(
      hosts(&["app", "--type=renderer", &switch, "--lang=en-US"]),
      ["apps.localhost", "other.localhost"]
    );
    // The last occurrence wins.
    let last = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=last.localhost");
    assert_eq!(hosts(&[&switch, &last]), ["last.localhost"]);
  }

  #[test]
  fn no_switch_leaves_every_frame_its_ipc() {
    assert!(hosts(&["app", "--type=renderer"]).is_empty());
    let empty = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=");
    assert!(hosts(&["app", &empty]).is_empty());
  }

  #[test]
  fn finds_the_host_of_a_url() {
    assert_eq!(
      url_host("http://apps.localhost/token/index.html"),
      Some("apps.localhost")
    );
    assert_eq!(url_host("https://apps.localhost"), Some("apps.localhost"));
    assert_eq!(
      url_host("http://apps.localhost:8080?x#y"),
      Some("apps.localhost")
    );
    assert_eq!(
      url_host("http://user:pw@apps.localhost/"),
      Some("apps.localhost")
    );
    assert_eq!(url_host("http://[::1]:8080/"), Some("::1"));
    assert_eq!(
      url_host("blob:http://apps.localhost/uuid"),
      Some("apps.localhost")
    );
    assert_eq!(url_host("about:blank"), None);
    assert_eq!(url_host("about:srcdoc"), None);
    assert_eq!(url_host("data:text/html,<p>"), None);
    assert_eq!(url_host("blob:null/uuid"), None);
    assert_eq!(url_host("file:///tmp/x"), None);
  }

  #[test]
  fn a_frame_on_the_hosts_stays_on_them() {
    let hosts = ["apps.localhost".to_string()];
    let app = "http://apps.localhost/token/index.html";
    assert!(!navigation_leaves_hosts(
      app,
      "http://apps.localhost/token/index.html",
      &hosts
    ));
    assert!(!navigation_leaves_hosts(
      app,
      "https://apps.localhost/other/",
      &hosts
    ));
    assert!(navigation_leaves_hosts(
      app,
      "data:text/html,<script>",
      &hosts
    ));
    assert!(navigation_leaves_hosts(app, "blob:null/uuid", &hosts));
    assert!(navigation_leaves_hosts(app, "about:blank", &hosts));
    assert!(navigation_leaves_hosts(
      app,
      "http://tauri.localhost/",
      &hosts
    ));
    // Frames elsewhere navigate freely, onto the hosts too.
    assert!(!navigation_leaves_hosts("about:blank", app, &hosts));
    assert!(!navigation_leaves_hosts(
      "http://tauri.localhost/",
      "data:text/html,",
      &hosts
    ));
    assert!(!navigation_leaves_hosts(app, "data:text/html,", &[]));
  }

  #[test]
  fn frames_on_or_under_the_hosts_have_no_ipc() {
    let hosts = ["apps.localhost".to_string()];
    let window = top("http://tauri.localhost/");
    let app = nested("http://apps.localhost/token/index.html", window.clone());
    assert!(!frame_without_ipc(&window, &hosts));
    assert!(frame_without_ipc(&app, &hosts));
    assert!(frame_without_ipc(
      &nested("about:srcdoc", app.clone()),
      &hosts
    ));
    assert!(frame_without_ipc(
      &nested("about:blank", nested("data:text/html,", app)),
      &hosts
    ));
    // A popup on the hosts, whoever opened it.
    assert!(frame_without_ipc(&top("http://apps.localhost/x"), &hosts));
    // Same-process frames elsewhere keep it.
    assert!(!frame_without_ipc(
      &nested("about:blank", window.clone()),
      &hosts
    ));
    assert!(!frame_without_ipc(&top("about:blank"), &hosts));
  }

  #[test]
  fn a_frame_whose_parent_is_in_another_process_has_no_ipc() {
    let hosts = ["apps.localhost".to_string()];
    // A cross-site frame an app embeds, and what it nests in turn: the renderer cannot see
    // the app above it.
    let embedded = out_of_process("https://maps.example/embed");
    assert!(frame_without_ipc(&embedded, &hosts));
    assert!(frame_without_ipc(&nested("about:srcdoc", embedded), &hosts));
    assert!(frame_without_ipc(
      &out_of_process("chrome-error://chromewebdata/"),
      &hosts
    ));
    // Without hosts, the walk never runs: see `on_context_created`.
  }

  #[test]
  fn matches_hosts_exactly() {
    let hosts = ["apps.localhost".to_string()];
    assert!(url_on_hosts("http://apps.localhost/token/", &hosts));
    assert!(url_on_hosts("https://APPS.localhost/", &hosts));
    assert!(!url_on_hosts("http://tauri.localhost/", &hosts));
    assert!(!url_on_hosts("http://evil-apps.localhost/", &hosts));
    assert!(!url_on_hosts("http://apps.localhost.evil/", &hosts));
    assert!(!url_on_hosts("about:blank", &hosts));
    assert!(!url_on_hosts("http://apps.localhost/", &[]));
  }
}

#[cfg(test)]
mod without_ipc_tests {
  use super::*;

  #[test]
  fn browser_and_popup_metadata_disable_ipc_and_preserve_other_fields() {
    crate::initialize_cef_test_api();
    let mut root = None;
    assert!(mark_without_ipc(&mut root));
    let mut popup = dictionary_value_create();
    assert_ne!(
      popup
        .as_ref()
        .unwrap()
        .set_int(Some(&"unrelated".into()), 42),
      0
    );
    assert!(mark_without_ipc(&mut popup));
    assert_eq!(popup.as_ref().unwrap().int(Some(&"unrelated".into())), 42);

    let mut browsers = IpcDisabledBrowsers::default();
    browsers.created(1, root.as_ref());
    browsers.created(2, popup.as_ref());
    browsers.created(3, None);
    assert!(browsers.contains(1));
    assert!(browsers.contains(2));
    assert!(!browsers.contains(3));

    // CEF can create the same browser ID again before its old instance goes away.
    browsers.created(1, root.as_ref());
    browsers.destroyed(1);
    assert!(browsers.contains(1));
    browsers.destroyed(1);
    assert!(!browsers.contains(1));
    assert!(browsers.contains(2));
  }
}
