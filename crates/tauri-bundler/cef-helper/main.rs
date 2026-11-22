// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cef::{args::Args, *};

const IPC_MESSAGE_NAME: &str = "tauri:ipc";
const IPC_POST_MESSAGE_FUNCTION: &str = "postMessage";

/// Switch the browser process uses to pass `Cef::frame_hosts_without_ipc` to renderers.
///
/// Mirrors `FRAME_HOSTS_WITHOUT_IPC_SWITCH` in `tauri-runtime-cef`, which appends it, along
/// with the parsing and frame checks below. This crate is built from standalone source and
/// cannot depend on that one, so keep both in sync.
const FRAME_HOSTS_WITHOUT_IPC_SWITCH: &str = "tauri-cef-frame-hosts-without-ipc";

/// The hosts the browser process passed, lowercased; empty if none. The last occurrence wins.
fn frame_hosts_without_ipc_from<I>(args: I) -> Vec<String>
where
  I: IntoIterator<Item = std::ffi::OsString>,
{
  let prefix = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=");
  let mut hosts = Vec::new();
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
  static HOSTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
  HOSTS.get_or_init(|| frame_hosts_without_ipc_from(std::env::args_os()))
}

/// The host of `url`, without port or brackets; for a `blob:` URL, the host of its origin.
fn url_host(url: &str) -> Option<&str> {
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

fn url_on_hosts(url: &str, hosts: &[String]) -> bool {
  url_host(url).is_some_and(|host| hosts.iter().any(|h| h.eq_ignore_ascii_case(host)))
}

/// Whether the frame or any frame it is nested in is on one of `hosts`, or may be: a frame
/// whose parent is in another renderer process counts, since this process cannot see it.
fn frame_without_ipc(frame: &Frame, hosts: &[String]) -> bool {
  let mut frame = Some(frame.clone());
  while let Some(current) = frame {
    if url_on_hosts(&CefString::from(&current.url()).to_string(), hosts) {
      return true;
    }
    frame = current.parent();
    if frame.is_none() && current.is_main() == 0 {
      return true;
    }
  }
  false
}

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
      if let Some(args) = process_message.as_ref().and_then(ProcessMessage::argument_list) {
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

// The standalone macOS helper mirrors these keys, origin helper, and browser tracking.
const WITHOUT_IPC_KEY: &str = "tauri_without_ipc";
const IPC_MAIN_FRAME_ONLY_KEY: &str = "tauri_ipc_main_frame_only";
const IPC_ALLOWED_ORIGINS_KEY: &str = "tauri_ipc_allowed_origins";
const IPC_HAS_REMOTE_URLS_KEY: &str = "tauri_ipc_has_remote_urls";

fn url_origin(url: &str) -> Option<String> {
  let (scheme, rest) = url.split_once("://")?;
  if scheme.is_empty()
    || !scheme
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
  {
    return None;
  }
  let scheme_lower = scheme.to_ascii_lowercase();
  if matches!(scheme_lower.as_str(), "blob" | "data" | "about" | "javascript" | "file") {
    return None;
  }
  let authority = rest.split(['/', '?', '#']).next()?;
  if authority.is_empty() || authority.contains('@') {
    return None;
  }
  let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
    let (h, after) = bracketed.split_once(']')?;
    if h.is_empty() {
      return None;
    }
    let port = match after {
      "" => None,
      p => Some(p.strip_prefix(':')?),
    };
    (format!("[{}]", h.to_ascii_lowercase()), port)
  } else if let Some((h, p)) = authority.rsplit_once(':') {
    if h.is_empty() {
      return None;
    }
    (h.to_ascii_lowercase(), Some(p))
  } else {
    (authority.to_ascii_lowercase(), None)
  };
  let port = match port {
    None | Some("") => None,
    Some(p) => {
      if !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
      }
      let is_default = (scheme_lower == "http" && p == "80")
        || (scheme_lower == "https" && p == "443");
      (!is_default).then_some(p)
    }
  };
  Some(match port {
    Some(p) => format!("{scheme_lower}://{host}:{p}"),
    None => format!("{scheme_lower}://{host}"),
  })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RendererIpcPolicy {
  AllFrames,
  Disabled,
  MainFrameOnly {
    allowed_origins: Vec<String>,
    has_remote_urls: bool,
  },
}

impl RendererIpcPolicy {
  fn from_extra_info(extra_info: Option<&DictionaryValue>) -> Self {
    let Some(info) = extra_info else {
      return Self::AllFrames;
    };
    if info.bool(Some(&CefString::from(WITHOUT_IPC_KEY))) != 0 {
      return Self::Disabled;
    }
    if info.bool(Some(&CefString::from(IPC_MAIN_FRAME_ONLY_KEY))) != 0 {
      let raw = CefString::from(&info.string(Some(&CefString::from(IPC_ALLOWED_ORIGINS_KEY))))
        .to_string();
      let allowed_origins = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
      let has_remote_urls = info.bool(Some(&CefString::from(IPC_HAS_REMOTE_URLS_KEY))) != 0;
      return Self::MainFrameOnly {
        allowed_origins,
        has_remote_urls,
      };
    }
    Self::AllFrames
  }

  fn allows_context(&self, is_main_frame: bool, window_origin: Option<&str>, frame_url: &str) -> bool {
    match self {
      Self::AllFrames => true,
      Self::Disabled => false,
      Self::MainFrameOnly {
        allowed_origins,
        has_remote_urls,
      } => {
        if !is_main_frame {
          return false;
        }
        let effective_origin = match window_origin {
          Some("null") => return false,
          Some(origin) if !origin.is_empty() => origin.to_ascii_lowercase(),
          _ => match url_origin(frame_url) {
            Some(origin) => origin,
            None => return false,
          },
        };
        if allowed_origins
          .iter()
          .any(|allowed| allowed.eq_ignore_ascii_case(&effective_origin))
        {
          return true;
        }
        *has_remote_urls
      }
    }
  }
}

#[derive(Default)]
struct BrowserIpcPolicies(HashMap<i32, (usize, RendererIpcPolicy)>);

impl BrowserIpcPolicies {
  fn created(&mut self, id: i32, extra_info: Option<&DictionaryValue>) {
    let incoming = RendererIpcPolicy::from_extra_info(extra_info);
    let entry = self.0.entry(id).or_insert((0, incoming.clone()));
    entry.0 += 1;
    if matches!(incoming, RendererIpcPolicy::Disabled)
      || (matches!(entry.1, RendererIpcPolicy::AllFrames)
        && !matches!(incoming, RendererIpcPolicy::AllFrames))
    {
      entry.1 = incoming;
    }
  }

  fn destroyed(&mut self, id: i32) {
    if let std::collections::hash_map::Entry::Occupied(mut entry) = self.0.entry(id) {
      entry.get_mut().0 -= 1;
      if entry.get().0 == 0 {
        entry.remove();
      }
    }
  }

  fn policy(&self, id: i32) -> RendererIpcPolicy {
    self
      .0
      .get(&id)
      .map(|(_, policy)| policy.clone())
      .unwrap_or(RendererIpcPolicy::AllFrames)
  }
}

fn browser_ipc_policies() -> &'static Mutex<BrowserIpcPolicies> {
  static BROWSERS: OnceLock<Mutex<BrowserIpcPolicies>> = OnceLock::new();
  BROWSERS.get_or_init(Mutex::default)
}

fn context_window_origin(context: Option<&mut V8Context>) -> Option<String> {
  let window = context?.global()?;
  let value = window.value_bykey(Some(&CefString::from("origin")))?;
  if value.is_string() != 0 {
    let origin = CefString::from(&value.string_value()).to_string();
    if !origin.is_empty() {
      return Some(origin);
    }
  }
  None
}

wrap_render_process_handler! {
  struct TauriRenderProcessHandler;

  impl RenderProcessHandler {
    fn on_browser_created(
      &self,
      browser: Option<&mut Browser>,
      extra_info: Option<&mut DictionaryValue>,
    ) {
      if let Some(browser) = browser {
        browser_ipc_policies()
          .lock()
          .unwrap()
          .created(browser.identifier(), extra_info.as_deref());
      }
    }

    fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
      if let Some(browser) = browser {
        browser_ipc_policies()
          .lock()
          .unwrap()
          .destroyed(browser.identifier());
      }
    }

    fn on_context_created(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      context: Option<&mut V8Context>,
    ) {
      let mut context = context;
      let Some(browser) = browser else {
        return;
      };
      let policy = browser_ipc_policies()
        .lock()
        .unwrap()
        .policy(browser.identifier());
      let Some(frame) = frame else {
        return;
      };
      let window_origin = context_window_origin(context.as_deref_mut());
      let frame_url = CefString::from(&frame.url()).to_string();
      if !policy.allows_context(
        frame.is_main() != 0,
        window_origin.as_deref(),
        &frame_url,
      ) {
        return;
      }
      let hosts = frame_hosts_without_ipc();
      if !hosts.is_empty() && frame_without_ipc(frame, hosts) {
        return;
      }
      install_ipc_post_message(context);
    }
  }
}

wrap_app! {
  struct TauriRenderApp;

  impl App {
    fn render_process_handler(&self) -> Option<RenderProcessHandler> {
      Some(TauriRenderProcessHandler::new())
    }
  }
}

/// Where the browser process says CEF was loaded from
#[cfg(target_os = "macos")]
const CEF_LIBRARY_PATH_ENV: &str = "TAURI_CEF_LIBRARY_PATH";

/// The framework named by that variable — the binary, or the `.framework`
/// directory holding it — or `None` for the bundle-relative location.
#[cfg(target_os = "macos")]
fn cef_framework_from_env() -> Option<std::path::PathBuf> {
  const FRAMEWORK_BINARY: &str = "Chromium Embedded Framework";

  let path = std::path::PathBuf::from(std::env::var_os(CEF_LIBRARY_PATH_ENV)?);
  Some(match path.extension() {
    Some(ext) if ext == "framework" => path.join(FRAMEWORK_BINARY),
    _ => path,
  })
}

/// Switch the browser process uses to pass its CEF API version to child processes.
///
/// Mirrors `API_VERSION_SWITCH` in `tauri-runtime-cef`, which appends it. This crate
/// is built from standalone source and cannot depend on that one, so keep both in sync.
const API_VERSION_SWITCH: &str = "tauri-cef-api-version";

/// The CEF API version the browser process passed to this child process.
///
/// `None` if this is not a child process (no `--type=` switch), if no version was
/// passed, or if the value is not a number. The last occurrence wins.
///
/// Must not call CEF: nothing may be called before the version is declared.
fn child_process_api_version<I>(args: I) -> Option<i32>
where
  I: IntoIterator<Item = std::ffi::OsString>,
{
  let prefix = format!("--{API_VERSION_SWITCH}=");
  let mut is_child_process = false;
  let mut version = None;
  // Non-Unicode arguments are skipped; `std::env::args` would panic on them.
  for arg in args {
    let Some(arg) = arg.to_str() else {
      continue;
    };
    if arg.starts_with("--type=") {
      is_child_process = true;
    } else if let Some(value) = arg.strip_prefix(&prefix) {
      version = value.parse().ok();
    }
  }
  version.filter(|_| is_child_process)
}

fn main() {
  let args = Args::new();

  #[cfg(target_os = "macos")]
  let cef_framework = cef_framework_from_env();

  // A framework outside the app bundle is out of the sandbox's reach, so
  // the browser process launched this one with `--no-sandbox` and entering
  // the sandbox here would only make the load below fail.
  #[cfg(all(target_os = "macos", feature = "sandbox"))]
  let _sandbox = cef_framework.is_none().then(|| {
    let mut sandbox = cef::sandbox::Sandbox::new();
    sandbox.initialize(args.as_main_args());
    sandbox
  });

  #[cfg(target_os = "macos")]
  match &cef_framework {
    // `load_library` directly: `LibraryLoader` only ever resolves the
    // bundle-relative location, and this framework is somewhere else.
    Some(path) => {
      use std::os::unix::ffi::OsStrExt;

      let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| panic!("CEF path is not a valid C string: {}", path.display()));
      if unsafe { load_library(Some(&*c_path.as_ptr().cast())) } != 1 {
        panic!("failed to load CEF from {}", path.display());
      }
    }
    None => {
      let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
      assert!(loader.load(), "failed to load CEF from the app bundle");
      // CEF's threads outlive every scope here and unloading is only safe
      // after `cef::shutdown`, so the loader (which unloads on drop) is
      // deliberately leaked.
      std::mem::forget(loader);
    }
  }

  // Declare the version the browser process passed, if any.
  let version = child_process_api_version(std::env::args_os()).unwrap_or(sys::CEF_API_VERSION_LAST);
  // `cef_api_hash` returns NULL if the loaded libcef does not support the version, and
  // the next CEF call would crash.
  if api_hash(version, 0).is_null() {
    panic!("the loaded CEF library does not support CEF API version {version}");
  }
  let mut app = TauriRenderApp::new();
  execute_process(
    Some(args.as_main_args()),
    Some(&mut app),
    std::ptr::null_mut(),
  );
}

#[cfg(test)]
mod tests {
  use super::{
    API_VERSION_SWITCH, FRAME_HOSTS_WITHOUT_IPC_SWITCH, RendererIpcPolicy,
    child_process_api_version, frame_hosts_without_ipc_from, url_on_hosts, url_origin,
  };

  #[test]
  fn renderer_ipc_policy_admits_only_non_opaque_allowed_main_frames() {
    let policy = RendererIpcPolicy::MainFrameOnly {
      allowed_origins: vec!["http://tauri.localhost".into()],
      has_remote_urls: false,
    };
    assert!(policy.allows_context(
      true,
      Some("http://tauri.localhost"),
      "http://tauri.localhost/"
    ));
    assert!(policy.allows_context(true, Some("http://tauri.localhost"), "about:blank"));
    assert!(!policy.allows_context(true, Some("null"), "http://tauri.localhost/"));
    assert!(!policy.allows_context(
      false,
      Some("http://tauri.localhost"),
      "http://tauri.localhost/child"
    ));
    assert!(!policy.allows_context(
      true,
      Some("http://apps.localhost:5555"),
      "http://apps.localhost:5555/report"
    ));
    assert_eq!(
      url_origin("http://Tauri.Localhost:80/index.html").as_deref(),
      Some("http://tauri.localhost")
    );
    assert_eq!(url_origin("data:text/html,hi"), None);
  }

  #[test]
  fn reads_the_frame_hosts_without_ipc() {
    let arg = format!("--{FRAME_HOSTS_WITHOUT_IPC_SWITCH}=Apps.localhost,other.localhost");
    let hosts = frame_hosts_without_ipc_from(
      ["app", "--type=renderer", arg.as_str()]
        .into_iter()
        .map(std::ffi::OsString::from),
    );
    assert_eq!(hosts, ["apps.localhost", "other.localhost"]);
    assert!(url_on_hosts("http://apps.localhost/token/", &hosts));
    assert!(url_on_hosts("blob:https://other.localhost/uuid", &hosts));
    assert!(!url_on_hosts("http://tauri.localhost/", &hosts));
    assert!(!url_on_hosts("about:srcdoc", &hosts));
  }

  fn version(args: &[&str]) -> Option<i32> {
    child_process_api_version(args.iter().map(std::ffi::OsString::from))
  }

  #[test]
  fn reads_the_version_the_browser_process_passed() {
    let arg = format!("--{API_VERSION_SWITCH}=15101");
    assert_eq!(version(&["app", "--type=renderer", &arg]), Some(15101));
    assert_eq!(version(&["app", &arg, "--type=gpu-process"]), Some(15101));
  }

  #[test]
  fn absent_or_garbage_means_the_default() {
    assert_eq!(version(&["app", "--type=renderer"]), None);
    assert_eq!(
      version(&["app", "--type=renderer", "--tauri-cef-api-version=latest"]),
      None
    );
  }

  #[test]
  fn a_browser_process_ignores_the_switch() {
    assert_eq!(version(&["app", "--tauri-cef-api-version=15101"]), None);
  }
}
