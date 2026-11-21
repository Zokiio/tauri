// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  fs::create_dir_all,
  path::{Component, Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use base64::Engine;
use cef::*;
use sha2::{Digest, Sha256};
use tauri_runtime::webview::WebviewAttributes;
use tauri_utils::Theme;

use crate::cef_impl::{preferences, request_handler};

#[inline]
fn theme_to_color_variant(theme: Option<Theme>) -> ColorVariant {
  match theme {
    Some(Theme::Dark) => ColorVariant::DARK,
    Some(Theme::Light) => ColorVariant::LIGHT,
    _ => ColorVariant::SYSTEM,
  }
}

pub(crate) fn apply_theme_scheme(request_context: Option<&RequestContext>, theme: Option<Theme>) {
  if let Some(request_context) = request_context {
    request_context.set_chrome_color_scheme(theme_to_color_variant(theme), 0);
  }
}

/// Resolves a CEF-compatible cache path for a per-webview request context.
///
/// CEF requires `RequestContextSettings.cache_path` to be either empty (which
/// puts the context in incognito mode) or an absolute path that is equal to,
/// or a child directory of, `Settings.root_cache_path` (which defaults to
/// `Settings.cache_path` when not set explicitly). Any value outside of that
/// root makes `request_context_create_context` (and downstream browser
/// creation) fail.
///
/// To support an arbitrary [`WebviewAttributes::data_directory`] while
/// honoring this constraint we:
///
/// * use the requested path directly when it already lives under the global
///   cache root, so callers that opt in to a path under the app cache get the
///   exact location they asked for;
/// * join relative paths without parent (`..`) components onto the root cache
///   path (typical short labels); and
/// * otherwise derive a stable direct child folder under `<root>/<hash>` from
///   the requested path, preserving isolation between webviews. Distinct
///   `data_directory` values produce distinct profiles, and the same value
///   maps to the same on-disk profile across runs.
fn resolve_request_context_cache_path(global_cache_path: &Path, data_directory: &Path) -> PathBuf {
  if data_directory.is_absolute() {
    if data_directory.starts_with(global_cache_path) {
      return data_directory.to_path_buf();
    } else {
      log::warn!(
        "data directory is not a child of the global cache path, we will derive a profile hash from it"
      );
    }
  } else if !data_directory
    .components()
    .any(|component| matches!(component, Component::ParentDir))
  {
    return global_cache_path.join(data_directory);
  } else {
    log::warn!(
      "data directory is a relative path with parent components, we will derive a profile hash from it"
    );
  }

  let mut hasher = Sha256::new();
  hasher.update(data_directory.as_os_str().as_encoded_bytes());
  let hash = hasher.finalize();
  let suffix = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash[..16]);
  let path = global_cache_path.join(format!("Profile-{suffix}"));
  log::info!(
    "derived profile hash from data directory: {suffix}, cache path: {}",
    path.display()
  );
  path
}

/// The profile directory a [`WebviewAttributes::data_store_identifier`] maps to, named by
/// the identifier spelled as the UUID WKWebView would show for it.
fn data_store_directory_name(identifier: &[u8; 16]) -> String {
  let hex: String = identifier
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  format!(
    "DataStore-{}-{}-{}-{}-{}",
    &hex[0..8],
    &hex[8..12],
    &hex[12..16],
    &hex[16..20],
    &hex[20..32]
  )
}

/// Continuation invoked on the CEF UI thread once the request context's
/// underlying browser context has finished asynchronous initialization.
///
/// Receives a fresh handle to the same [`RequestContext`] that was created in
/// [`request_context_from_webview_attributes`], so the continuation can pass
/// it to `browser_view_create` / `browser_host_create_browser_sync` knowing
/// that `VerifyBrowserContext()` will succeed.
pub(crate) type RequestContextInitContinuation = Box<dyn FnOnce(Option<RequestContext>) + 'static>;

/// Wraps a deferred-init continuation so that it always flips a shared
/// completion flag when it exits, regardless of how it exits (normal return,
/// early `return` on browser-create failure, or panic).
///
/// Returns the completion flag plus the wrapped continuation.
pub(crate) fn deferred_init_continuation<F>(
  work: F,
) -> (Arc<AtomicBool>, RequestContextInitContinuation)
where
  F: FnOnce(Option<RequestContext>) + 'static,
{
  struct Guard(Arc<AtomicBool>);
  impl Drop for Guard {
    fn drop(&mut self) {
      self.0.store(true, Ordering::SeqCst);
    }
  }

  let flag = Arc::new(AtomicBool::new(false));
  let guard = Guard(flag.clone());
  let wrapped: RequestContextInitContinuation = Box::new(move |request_context| {
    let _guard = guard;
    work(request_context);
  });
  (flag, wrapped)
}

/// Block the calling thread until `flag` is `true`.
///
/// Browser creation goes through `RequestContextHandler::on_request_context_initialized`,
/// which CEF always dispatches via `CEF_POST_TASK(CEF_UIT, ...)`. Tauri runs
/// CEF with an external message pump (see `cef::do_message_loop_work` in the
/// runtime's main loop), so the only way for that posted task to actually
/// execute is for someone on the CEF UI thread to keep pumping the loop.
///
/// Two cases:
///
/// 1. We're on the CEF UI thread (typical: app setup, dispatched messages, or
///    inside a CEF callback like `LifeSpanHandler::on_after_created` /
///    `RequestHandler::on_open_url_from_tab`). Pump the message loop ourselves
///    so the `OnRequestContextInitialized` task can run.
///
///    We must enable nestable tasks for the duration of the pump because we
///    may already be running inside another CEF task; without
///    `CefSetNestableTasksAllowed(true)` Chromium's `RunLoop::RunUntilIdle`
///    refuses to dispatch any task to the UI thread, the deferred init never
///    fires, and we'd spin here forever.
///
/// 2. We're on some other thread (e.g. a tokio IPC handler that called the
///    Tauri API directly). The CEF UI thread is running its own pump and will
///    pick up our queued init task on its own; we just block here on a sleep
///    loop until the flag flips. We can't call `do_message_loop_work` from
///    this thread - it asserts on the init thread.
///
/// Spinning here keeps `create_webview` synchronous from the caller's
/// perspective: the function does not return until the browser exists in
/// `state.windows`, so any subsequent dispatcher call (e.g.
/// `webview.open_devtools()`, `webview.on_dev_tools_protocol(...)`) can find
/// the webview.
pub(crate) fn wait_for_deferred_init(flag: &Arc<AtomicBool>) {
  let on_ui_thread = cef::currently_on(cef::sys::cef_thread_id_t::TID_UI.into()) != 0;

  if on_ui_thread {
    let _allow = AllowNestableTasks::enter();
    while !flag.load(Ordering::SeqCst) {
      cef::do_message_loop_work();
    }
  } else {
    while !flag.load(Ordering::SeqCst) {
      std::thread::sleep(Duration::from_millis(1));
    }
  }
}

/// RAII guard that scopes `CefSetNestableTasksAllowed(true)` for the current
/// CEF UI-thread call.
///
/// CEF requires balanced enable/disable calls and explicitly forbids
/// reentrancy at the C++ level (`CHECK(allowed != has_value())`). The guard
/// uses a thread-local depth counter so only the outermost
/// [`wait_for_deferred_init`] on this thread toggles the flag, which makes
/// nesting (e.g. an `on_initialized` continuation that creates another
/// webview) safe.
struct AllowNestableTasks;

impl AllowNestableTasks {
  fn enter() -> Self {
    NESTABLE_TASKS_DEPTH.with(|depth| {
      let current = depth.get();
      if current == 0 {
        cef::set_nestable_tasks_allowed(1);
      }
      depth.set(current + 1);
    });
    Self
  }
}

impl Drop for AllowNestableTasks {
  fn drop(&mut self) {
    NESTABLE_TASKS_DEPTH.with(|depth| {
      let current = depth.get();
      depth.set(current - 1);
      if current == 1 {
        cef::set_nestable_tasks_allowed(0);
      }
    });
  }
}

thread_local! {
  static NESTABLE_TASKS_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

wrap_request_context_handler! {
  struct WebviewRequestContextHandler {
    on_initialized: Arc<Mutex<Option<RequestContextInitContinuation>>>,
    before_resource_load_handler: Option<Arc<crate::BeforeResourceLoadHandler>>,
  }

  impl RequestContextHandler {
    fn on_request_context_initialized(&self, request_context: Option<&mut RequestContext>) {
      let Some(callback) = self.on_initialized.lock().unwrap().take() else {
        return;
      };
      let request_context = request_context.map(|rc| rc.clone());
      callback(request_context);
    }
    fn resource_request_handler(
      &self,
      _browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      _request: Option<&mut Request>,
      _is_navigation: i32,
      _is_download: i32,
      _request_initiator: Option<&CefString>,
      _disable_default_handling: Option<&mut i32>,
    ) -> Option<ResourceRequestHandler> {
      request_handler::guarded_resource_handler(self.before_resource_load_handler.clone(), None)
    }
  }
}

wrap_completion_callback! {
  struct ProxyConnectionsClosed {
    ready: Arc<Mutex<Option<Box<dyn FnOnce()>>>>,
  }

  impl CompletionCallback {
    fn on_complete(&self) {
      let ready = self.ready.lock().unwrap().take();
      if let Some(ready) = ready {
        ready();
      }
    }
  }
}

fn install_proxy(
  proxy_url: &url::Url,
  bypass_list: Option<&str>,
  set: impl FnOnce(&serde_json::Value) -> bool,
  close: impl FnOnce(),
) -> bool {
  let scheme = match proxy_url.scheme() {
    "socks5" | "socks5h" => "socks5",
    "socks4" | "socks4a" => "socks4",
    "https" => "https",
    _ => "http",
  };
  let Some(host) = proxy_url.host_str() else {
    return false;
  };
  let server = match proxy_url.port_or_known_default() {
    Some(port) => format!("{scheme}://{host}:{port}"),
    None => format!("{scheme}://{host}"),
  };
  let mut preference = serde_json::json!({ "mode": "fixed_servers", "server": server });
  if let Some(bypass_list) = bypass_list {
    preference["bypass_list"] = bypass_list.into();
  }
  if !set(&preference) {
    return false;
  }
  close();
  true
}

/// Installs the proxy and waits for Chromium to close existing connections
/// before the webview can navigate.
fn apply_proxy(
  request_context: RequestContext,
  proxy_url: &url::Url,
  bypass_list: Option<&str>,
  ready: RequestContextInitContinuation,
) {
  let ready = Arc::new(Mutex::new(Some(ready)));
  if !install_proxy(
    proxy_url,
    bypass_list,
    |value| preferences::set_preference(&request_context, "proxy", value),
    || {
      let context = request_context.clone();
      let ready = ready.clone();
      let mut callback =
        ProxyConnectionsClosed::new(Arc::new(Mutex::new(Some(Box::new(move || {
          let ready = ready.lock().unwrap().take();
          if let Some(ready) = ready {
            ready(Some(context));
          }
        })))));
      request_context.close_all_connections(Some(&mut callback));
    },
  ) {
    log::warn!("failed to apply the proxy preference to the CEF request context");
    if let Some(ready) = ready.lock().unwrap().take() {
      // Preserve the existing behavior for portable proxy_url callers.
      ready(bypass_list.is_none().then_some(request_context));
    }
  }
}

fn request_context_settings(
  global_cache_path: &Path,
  webview_attributes: &WebviewAttributes,
) -> RequestContextSettings {
  let own_cache_path = |cache_path: PathBuf| {
    if let Err(error) = create_dir_all(&cache_path) {
      log::error!(
        "failed to create request context cache directory {}: {error}",
        cache_path.display()
      );
    }
    CefStringUtf16::from(cache_path.to_string_lossy().as_ref())
  };

  let own_incognito = webview_attributes.without_ipc
    && webview_attributes.data_directory.is_none()
    && webview_attributes.data_store_identifier.is_none();
  let cache_path = if webview_attributes.incognito || own_incognito {
    CefStringUtf16::from("")
  } else if let Some(data_directory) = &webview_attributes.data_directory {
    own_cache_path(resolve_request_context_cache_path(
      global_cache_path,
      data_directory,
    ))
  } else if let Some(identifier) = &webview_attributes.data_store_identifier {
    // WKWebView's `WKWebsiteDataStore(forIdentifier:)` names a persistent store by UUID.
    // CEF's separate store is a separate request context cache path, so the identifier
    // names a profile directory under the root the way a relative `data_directory` does.
    // `data_directory` wins when both are set: an application sets both to cover the
    // platforms each one is for, and either gives it the same isolation here.
    own_cache_path(global_cache_path.join(data_store_directory_name(identifier)))
  } else {
    let global_context =
      request_context_get_global_context().expect("Failed to get global request context");
    // global_cache_path does not work here - global_context.cache_path() returns the proper profile path.
    (&global_context.cache_path()).into()
  };

  RequestContextSettings {
    cache_path,
    ..Default::default()
  }
}

fn create_webview_request_context(
  global_cache_path: &Path,
  attributes: &WebviewAttributes,
  shared: Option<RequestContext>,
  handler: &mut RequestContextHandler,
) -> Option<RequestContext> {
  if let Some(mut shared) = shared {
    let is_incognito = CefString::from(&shared.cache_path()).to_string().is_empty();
    let shares_global = attributes.without_ipc
      && request_context_get_global_context()
        .is_some_and(|global| global.is_sharing_with(Some(&mut shared)) != 0);
    if let Err(reason) = validate_shared_context(attributes, is_incognito, shares_global) {
      log::error!("{reason}");
      return None;
    }
    return request_context_cef_create_context_shared(Some(&mut shared), Some(handler));
  }
  let settings = request_context_settings(global_cache_path, attributes);
  request_context_create_context(Some(&settings), Some(handler))
}

fn validate_shared_context(
  attributes: &WebviewAttributes,
  is_incognito: bool,
  shares_global: bool,
) -> Result<(), &'static str> {
  if attributes.incognito && !is_incognito {
    return Err("an incognito webview requires an incognito CEF request context");
  }
  if attributes.without_ipc && shares_global {
    return Err("a webview with IPC disabled cannot share the global CEF request context");
  }
  Ok(())
}

/// Creates a per-webview [`RequestContext`], registers Tauri's custom URI
/// scheme handler factories on it, and arranges for `on_initialized` to fire
/// once the underlying Chromium `Profile` is fully created.
///
/// CEF only synchronously initializes the request context when its `cache_path`
/// equals `Settings.root_cache_path` (it then reuses the global "Default"
/// profile via `GetPrimaryUserProfile()`) or when the cache_path is empty
/// (off-the-record profile). Any other path (notably the per-`data_directory`
/// case used by Tauri) takes `ChromeBrowserContext::InitializeAsync`'s
/// `CreateProfileAsync` branch which finishes asynchronously. Calling
/// `browser_host_create_browser_sync` synchronously after
/// `request_context_create_context` would then fail
/// `CefRequestContextImpl::VerifyBrowserContext()` and return a null browser.
///
/// Routing browser creation through `on_initialized` keeps a single code path
/// for every cache_path layout: CEF always dispatches the callback through
/// `CEF_POST_TASK(CEF_UIT, ...)`, so even the synchronous-init cases are
/// handled by the same continuation.
///
/// Scheme handler factories are registered here, synchronously after
/// `request_context_create_context` returns, and *before* the
/// `OnRequestContextInitialized` task that drives browser creation is
/// dispatched. `RegisterSchemeHandlerFactory` internally queues its work
/// behind the request context's initialization (`StoreOrTriggerInitCallback`
/// when the browser context is not yet initialized, or an immediate UI -> IO
/// hop otherwise), so by the time the browser finally issues its first
/// navigation against any of these schemes the factories have been wired up
/// on the IO thread.
pub(crate) fn request_context_from_webview_attributes<'a>(
  global_cache_path: &Path,
  webview_attributes: &WebviewAttributes,
  shared_request_context: Option<RequestContext>,
  proxy_bypass_list: Option<String>,
  before_resource_load_handler: Option<Arc<crate::BeforeResourceLoadHandler>>,
  profile_preferences: Arc<Vec<(String, serde_json::Value)>>,
  content_settings: Arc<Vec<(ContentSettingTypes, ContentSettingValues)>>,
  custom_schemes: impl IntoIterator<Item = &'a String>,
  custom_protocol_scheme: &str,
  scheme_registry: request_handler::SchemeRegistry,
  main_thread: request_handler::MainThreadDispatcher,
  frame_hosts_without_ipc: Arc<Vec<String>>,
  on_initialized: RequestContextInitContinuation,
) -> Option<RequestContext> {
  // Holds a strong reference to the `RequestContext` until the
  // `on_request_context_initialized` callback fires. CEF keeps the underlying
  // C++ `CefRequestContextImpl` alive during async profile creation through
  // its own bound callbacks, but holding an explicit reference here guarantees
  // we don't race with reference-count releases on shutdown paths.
  let rc_holder: Arc<Mutex<Option<RequestContext>>> = Arc::new(Mutex::new(None));
  let proxy_url = webview_attributes.proxy_url.clone();
  let wrapped_callback: RequestContextInitContinuation = Box::new({
    let rc_holder = rc_holder.clone();
    move |rc| {
      let ready: RequestContextInitContinuation = Box::new(move |rc| {
        on_initialized(rc);
        let _released = rc_holder.lock().unwrap().take();
      });
      if let Some(rc) = rc.as_ref() {
        preferences::apply_app_webview_preferences(rc, &profile_preferences);
        preferences::apply_default_content_settings(rc, &content_settings);
        if let Some(proxy_url) = proxy_url.as_ref() {
          apply_proxy(rc.clone(), proxy_url, proxy_bypass_list.as_deref(), ready);
          return;
        }
      }
      ready(rc);
    }
  });

  let mut handler = WebviewRequestContextHandler::new(
    Arc::new(Mutex::new(Some(wrapped_callback))),
    before_resource_load_handler,
  );
  let request_context = create_webview_request_context(
    global_cache_path,
    webview_attributes,
    shared_request_context,
    &mut handler,
  );
  *rc_holder.lock().unwrap() = request_context.clone();

  if !webview_attributes.without_ipc
    && let Some(request_context) = request_context.as_ref()
  {
    for scheme in custom_schemes {
      request_context.register_scheme_handler_factory(
        Some(&custom_protocol_scheme.into()),
        Some(&format!("{scheme}.localhost").as_str().into()),
        Some(&mut request_handler::UriSchemeHandlerFactory::new(
          scheme_registry.clone(),
          scheme.clone(),
          main_thread.clone(),
          frame_hosts_without_ipc.clone(),
        )),
      );
    }
  }

  request_context
}

#[cfg(test)]
mod data_store_tests {
  use super::*;

  #[test]
  fn proxy_sets_bypass_rules_before_closing_connections() {
    use std::cell::Cell;

    let state = Cell::new(0);
    let url = "http://127.0.0.1:8080".parse().unwrap();
    assert!(install_proxy(
      &url,
      Some("*,<-loopback>"),
      |value| {
        assert_eq!(
          value,
          &serde_json::json!({
            "mode": "fixed_servers",
            "server": "http://127.0.0.1:8080",
            "bypass_list": "*,<-loopback>",
          })
        );
        assert_eq!(state.replace(1), 0);
        true
      },
      || assert_eq!(state.replace(2), 1),
    ));
    assert_eq!(state.get(), 2);

    assert!(!install_proxy(
      &url,
      None,
      |value| {
        assert!(value.get("bypass_list").is_none());
        false
      },
      || panic!("connections were closed after a rejected preference")
    ));
  }

  #[test]
  fn request_context_installs_worker_guard_without_a_browser() {
    crate::initialize_cef_test_api();
    let handler = WebviewRequestContextHandler::new(
      Arc::new(Mutex::new(None)),
      Some(Arc::new(|_| crate::BeforeResourceLoad::Cancel)),
    );
    let guard = handler
      .resource_request_handler(None, None, None, 0, 0, None, None)
      .expect("worker handler");
    let mut request = cef::request_create().unwrap();
    request.set_url(Some(&CefString::from("http://localhost/")));
    assert_eq!(
      guard.on_before_resource_load(None, None, Some(&mut request), None),
      cef::sys::cef_return_value_t::RV_CANCEL.into(),
    );
  }

  #[test]
  fn incognito_webviews_can_share_only_an_off_the_record_profile() {
    let mut attributes = WebviewAttributes::new(tauri_utils::config::WebviewUrl::External(
      "https://example.test/".parse().unwrap(),
    ))
    .incognito(true);

    assert!(validate_shared_context(&attributes, true, false).is_ok());
    assert!(validate_shared_context(&attributes, false, false).is_err());

    attributes.without_ipc = true;
    assert!(validate_shared_context(&attributes, true, true).is_err());
    assert!(validate_shared_context(&attributes, true, false).is_ok());
  }

  #[test]
  fn a_data_store_identifier_names_a_uuid_shaped_directory() {
    let identifier = [
      0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
      0xef,
    ];
    assert_eq!(
      data_store_directory_name(&identifier),
      "DataStore-12345678-9abc-def0-0123-456789abcdef"
    );
  }

  #[test]
  fn distinct_identifiers_name_distinct_directories() {
    assert_ne!(
      data_store_directory_name(&[0; 16]),
      data_store_directory_name(&[1; 16])
    );
  }
}
