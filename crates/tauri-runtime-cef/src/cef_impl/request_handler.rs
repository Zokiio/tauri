// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  borrow::Cow,
  io::{Cursor, Read},
  sync::{Arc, Mutex},
};

use cef::{rc::*, *};
use dioxus_debug_cell::RefCell;
use html5ever::{LocalName, interface::QualName, namespace_url, ns};
use http::{
  HeaderMap, HeaderName, HeaderValue,
  header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, ORIGIN},
};
use kuchiki::{Attribute, ExpandedName, NodeRef};
use tauri_runtime::{
  UserEvent,
  webview::{NavigationHandler, UriSchemeProtocolHandler},
  window::WindowId,
};
use tauri_utils::{
  config::{Csp, CspDirectiveSources},
  html::{parse as parse_html, serialize_node},
};
use url::Url;

use crate::{
  cef_impl::client::{
    DragDropEventTarget, DragDropState, WebDragDropResourceRequestHandler,
    WebDragDropResourceRequestHandlerArgs,
  },
  macros::wrap_with_args,
  runtime::RuntimeContext,
  webview::{CefInitScript, INITIAL_LOAD_URL},
};

type HttpResponse = Arc<RefCell<Option<http::Response<Cursor<Vec<u8>>>>>>;
/// Runs a closure on the runtime's main thread, see [`WebResourceHandler`]. Type-erased so
/// the CEF-side handlers, which are not generic over the user event type, can carry it.
pub(crate) type MainThreadDispatcher = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;
pub(crate) type SchemeRegistry = Arc<
  Mutex<
    std::collections::HashMap<
      (i32, String),
      (
        String,
        Arc<Box<UriSchemeProtocolHandler>>,
        Arc<Vec<CefInitScript>>,
        Option<Arc<tauri_runtime::webview::IpcAccessPolicy>>,
      ),
    >,
  >,
>;

fn csp_inject_initialization_scripts_hashes(
  existing_csp: String,
  initialization_scripts: &[CefInitScript],
  is_main_frame: bool,
) -> String {
  if initialization_scripts.is_empty() {
    return existing_csp;
  }

  let script_hashes: Vec<String> = initialization_scripts
    .iter()
    .filter(|script| script.runs_in_frame(is_main_frame))
    .map(|s| s.hash.clone())
    .collect();

  if script_hashes.is_empty() {
    return existing_csp;
  }

  let mut csp_map: std::collections::HashMap<String, CspDirectiveSources> =
    Csp::Policy(existing_csp.to_string()).into();

  let script_src = csp_map
    .entry("script-src".to_string())
    .or_insert_with(|| CspDirectiveSources::List(vec!["'self'".to_string()]));

  script_src.extend(script_hashes);

  Csp::DirectiveMap(csp_map).to_string()
}

fn inject_scripts_into_html_body(
  body: &[u8],
  initialization_scripts: &[CefInitScript],
  is_main_frame: bool,
) -> Option<Vec<u8>> {
  let Ok(body_str) = std::str::from_utf8(body) else {
    return None;
  };

  let document = parse_html(body_str.to_string());

  let head = if let Ok(ref head_node) = document.select_first("head") {
    head_node.as_node().clone()
  } else {
    let head_node = NodeRef::new_element(
      QualName::new(None, ns!(html), LocalName::from("head")),
      None,
    );
    document.prepend(head_node.clone());
    head_node
  };

  for init_script in initialization_scripts
    .iter()
    .rev()
    .filter(|script| script.runs_in_frame(is_main_frame))
  {
    let script_el = NodeRef::new_element(QualName::new(None, ns!(html), "script".into()), None);
    script_el.append(NodeRef::new_text(init_script.script.as_str()));
    head.prepend(script_el);
  }

  keep_encoding_declaration_first(&document, &head);

  Some(serialize_node(&document))
}

/// Keeps the document's character encoding declaration ahead of the injected scripts.
///
/// Chromium only pre-scans the first 1024 bytes of a document for a `<meta charset>`
/// declaration. The initialization scripts we prepend to `<head>` are much larger than
/// that, so an encoding declaration that used to be in the pre-scan window ends up out
/// of reach and the document is decoded with the fallback encoding (windows-1252)
/// instead. That mojibakes every non-ASCII byte in the page and in the injected scripts,
/// which additionally breaks the CSP hashes we compute over the UTF-8 source, so the
/// browser refuses to execute the affected script.
fn keep_encoding_declaration_first(document: &NodeRef, head: &NodeRef) {
  let existing_declaration = document.select("meta").ok().and_then(|metas| {
    metas.into_iter().find(|meta| {
      let attributes = meta.attributes.borrow();
      attributes.get("charset").is_some()
        || attributes
          .get("http-equiv")
          .map(|value| value.trim().eq_ignore_ascii_case("content-type"))
          .unwrap_or(false)
    })
  });

  match existing_declaration {
    // the document declares its encoding, move that declaration back into the pre-scan window
    Some(declaration) => {
      let node = declaration.as_node().clone();
      node.detach();
      head.prepend(node);
    }
    // no declaration at all: the assets are served as UTF-8 (this handler parses them as
    // such), so make that explicit instead of leaving it to the fallback encoding
    None => head.prepend(NodeRef::new_element(
      QualName::new(None, ns!(html), LocalName::from("meta")),
      [(
        ExpandedName::new(ns!(), LocalName::from("charset")),
        Attribute {
          prefix: None,
          value: "utf-8".into(),
        },
      )],
    )),
  }
}

fn is_app_protocol_url(url: &Url, protocols: &[String]) -> bool {
  if matches!(url.scheme(), "http" | "https") {
    url
      .host_str()
      .map(|host| host.trim_end_matches('.'))
      .and_then(|host| host.strip_suffix(".localhost"))
      .is_some_and(|scheme| {
        protocols
          .iter()
          .any(|name| name.eq_ignore_ascii_case(scheme))
      })
  } else {
    protocols
      .iter()
      .any(|name| name.eq_ignore_ascii_case(url.scheme()))
  }
}

fn answer_proxy_challenge(
  handler: Option<&crate::AuthCredentialsHandler>,
  is_proxy: bool,
  host: Option<&CefString>,
  port: i32,
  realm: Option<&CefString>,
  scheme: Option<&CefString>,
  answer: impl FnOnce(&str, &str),
) -> i32 {
  let (true, Some(handler), Some(host), Ok(port)) = (is_proxy, handler, host, u16::try_from(port))
  else {
    return 0;
  };
  let host = host.to_string();
  let realm = realm.map(ToString::to_string).unwrap_or_default();
  let scheme = scheme.map(ToString::to_string).unwrap_or_default();
  let Some((username, password)) = handler(&host, port, &realm, &scheme) else {
    return 0;
  };
  answer(&username, &password);
  1
}

wrap_with_args! {
  wrap_request_handler => WebRequestHandlerArgs;

  pub struct WebRequestHandler<T: UserEvent> {
    navigation_handler: Option<Arc<NavigationHandler>>,
    auth_credentials_handler: Option<Arc<crate::AuthCredentialsHandler>>,
    before_resource_load_handler: Option<Arc<crate::BeforeResourceLoadHandler>>,
    frame_event_handler: Option<Arc<crate::FrameEventHandler>>,
    context: RuntimeContext<T>,
    window_id: WindowId,
    webview_id: u32,
    drag_drop_event_target: DragDropEventTarget,
    drag_drop_handler_enabled: bool,
    drag_drop_state: Arc<Mutex<DragDropState>>,
    web_content_process_terminate_handler: Option<Arc<tauri_runtime::webview::OnWebContentProcessTerminateHandler>>,
    certificate_errors: crate::CertificateErrorPolicy,
    without_ipc: bool,
    blocked_app_protocols: Arc<Vec<String>>,
  }

  impl RequestHandler {
    fn auth_credentials(
      &self,
      _browser: Option<&mut Browser>,
      _origin_url: Option<&CefString>,
      is_proxy: i32,
      host: Option<&CefString>,
      port: i32,
      realm: Option<&CefString>,
      scheme: Option<&CefString>,
      callback: Option<&mut AuthCallback>,
    ) -> i32 {
      let Some(callback) = callback else { return 0 };
      answer_proxy_challenge(
        self.auth_credentials_handler.as_deref(),
        is_proxy != 0,
        host,
        port,
        realm,
        scheme,
        |username, password| callback.cont(
          Some(&CefString::from(username)),
          Some(&CefString::from(password)),
        ),
      )
    }

    /// Answers a TLS certificate that does not validate, per
    /// [`CertificateErrorPolicy`](crate::CertificateErrorPolicy).
    ///
    /// Returning 0 hands the error back to Chromium, which shows the SSL interstitial
    /// with its "proceed anyway" link. Cancelling instead means dropping the callback
    /// without continuing it: CEF reads a `false` return as "cancel this request", and
    /// the page gets a network error it cannot click past.
    fn on_certificate_error(
      &self,
      _browser: Option<&mut Browser>,
      cert_error: Errorcode,
      request_url: Option<&CefString>,
      _ssl_info: Option<&mut Sslinfo>,
      _callback: Option<&mut Callback>,
    ) -> ::std::os::raw::c_int {
      match self.certificate_errors {
        crate::CertificateErrorPolicy::ChromeInterstitial => 0,
        crate::CertificateErrorPolicy::Cancel => {
          // The URL can carry credentials and tokens, so only its origin is logged.
          let origin = request_url
            .map(ToString::to_string)
            .and_then(|url| url::Url::parse(&url).ok())
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_else(|| "an unknown origin".to_string());
          log::warn!(
            "cancelling a request to {origin}: its TLS certificate did not validate \
             (Chromium error {cert_error:?}), and the runtime is set to \
             CertificateErrorPolicy::Cancel"
          );
          1
        }
      }
    }
    fn on_render_process_terminated(
      &self,
      browser: Option<&mut Browser>,
      status: TerminationStatus,
      error_code: ::std::os::raw::c_int,
      error_string: Option<&CefString>,
    ) {
      let mut frame = browser.as_ref().and_then(|browser| browser.main_frame());
      crate::frame::emit_frame_event(&self.frame_event_handler, browser, frame.as_mut(), crate::FrameEventKind::RendererTerminated);
      if let Some(handler) = &self.web_content_process_terminate_handler {
        handler(tauri_runtime::webview::WebContentProcessTermination {
          reason: termination_reason(status),
          error_code: Some(error_code),
          error_string: error_string.map(ToString::to_string),
        });
      }
    }

    fn on_before_browse(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      request: Option<&mut Request>,
      _user_gesture: ::std::os::raw::c_int,
      _is_redirect: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int {
      let _ = (&self.context, self.window_id, self.webview_id);

      let Some(frame) = frame else {
        return 0;
      };
      let Some(request) = request else {
        return 0;
      };

      let url_str = CefString::from(&request.url()).to_string();

      if url_str == INITIAL_LOAD_URL {
        return 0;
      }

      let Ok(url) = url::Url::parse(&url_str) else {
        return 0;
      };

      if self.without_ipc
        && is_app_protocol_url(&url, &self.blocked_app_protocols)
      {
        return 1;
      }

      // A frame on a host left without `window.ipc` stays on those hosts, so its document
      // cannot navigate it to one it wrote (`data:`, `blob:`), which would get the bridge.
      // See `Cef::frame_hosts_without_ipc`.
      if crate::cef_impl::ipc::navigation_leaves_hosts(
        &CefString::from(&frame.url()).to_string(),
        &url_str,
        &self.context.frame_hosts_without_ipc,
      ) {
        log::debug!("cancelled a navigation off a host without `window.ipc`");
        return 1;
      }

      // Preserve the portable main-frame policy. Native observers receive only
      // admitted navigations, so a denied navigation cannot strand their barrier.
      if frame.is_main() != 0
        && self.navigation_handler.as_ref().is_some_and(|handler| !handler(&url))
      {
        return 1;
      }
      crate::frame::emit_frame_event(
        &self.frame_event_handler,
        browser,
        Some(frame),
        crate::FrameEventKind::NavigationStarted { url },
      );
      0
    }

    fn resource_request_handler(
      &self,
      _browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      _request: Option<&mut Request>,
      _is_navigation: ::std::os::raw::c_int,
      _is_download: ::std::os::raw::c_int,
      _request_initiator: Option<&CefString>,
      _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
    ) -> Option<ResourceRequestHandler> {
      let drag_handler = self.drag_drop_handler_enabled.then(|| {
        WebDragDropResourceRequestHandler::build(WebDragDropResourceRequestHandlerArgs {
          context: self.context.clone(),
          window_id: self.window_id,
          webview_id: self.webview_id,
          drag_drop_event_target: self.drag_drop_event_target,
          drag_drop_handler_enabled: self.drag_drop_handler_enabled,
          drag_drop_state: self.drag_drop_state.clone(),
        })
      });
      guarded_resource_handler(self.before_resource_load_handler.clone(), drag_handler)
    }
  }
}

pub(crate) fn guarded_resource_handler(
  policy: Option<Arc<crate::BeforeResourceLoadHandler>>,
  next: Option<ResourceRequestHandler>,
) -> Option<ResourceRequestHandler> {
  match policy {
    Some(policy) => Some(GuardedResourceHandler::new(policy, next)),
    None => next,
  }
}

wrap_resource_request_handler! {
  struct GuardedResourceHandler {
    policy: Arc<crate::BeforeResourceLoadHandler>,
    next: Option<ResourceRequestHandler>,
  }

  impl ResourceRequestHandler {
    fn on_before_resource_load(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      request: Option<&mut Request>,
      callback: Option<&mut Callback>,
    ) -> ReturnValue {
      if let Some(request) = request.as_ref()
        && let Ok(url) = Url::parse(&CefString::from(&request.url()).to_string())
      {
        match (self.policy)(&url) {
          crate::BeforeResourceLoad::Continue => {}
          crate::BeforeResourceLoad::Redirect(url) => {
            request.set_url(Some(&CefString::from(url.as_str())));
            return sys::cef_return_value_t::RV_CONTINUE.into();
          }
          crate::BeforeResourceLoad::Cancel => return sys::cef_return_value_t::RV_CANCEL.into(),
        }
      }
      match &self.next {
        Some(next) => next.on_before_resource_load(browser, frame, request, callback),
        None => sys::cef_return_value_t::RV_CONTINUE.into(),
      }
    }
  }
}

wrap_with_args! {
  wrap_resource_handler => WebResourceHandlerArgs;

  pub struct WebResourceHandler {
    webview_label: String,
    handler: Arc<Box<UriSchemeProtocolHandler>>,
    initialization_scripts: Arc<Vec<CefInitScript>>,
    // Whether the document this response is loaded into is the main frame. Scripts flagged
    // `for_main_frame_only` are skipped for subframes - the isolation iframe most notably -
    // matching both the wry runtime and the guard in the document-start script we register
    // over CDP for documents that are not served by a custom protocol.
    is_main_frame: bool,
    // Serialized origin of the main frame that initiated this request, captured
    // browser-side in the scheme handler factory. The renderer can issue an IPC
    // request before its execution context is fully wired to the loader; in
    // that window Chromium tags the request with `Origin: null` even though the
    // document already has a proper origin. We use this to repair the `Origin`
    // header in that case. `None` when the initiator is not the (non-opaque)
    // main frame, so sandboxed/subframe `Origin: null` requests are left as-is.
    initiator_origin: Option<String>,
    // Runs the protocol handler on the main thread, see `process_request`.
    main_thread: MainThreadDispatcher,
    // Filled in by the responder on the main thread, read back on the IO thread.
    response: HttpResponse,
  }

  impl ResourceHandler {
    /// Answers a request for a custom scheme with the app's protocol handler.
    ///
    /// CEF calls this on its IO thread; the handler is run on the main thread instead,
    /// which is where the wry runtime's handlers run (the platform webviews call their
    /// scheme handlers there) and what Tauri relies on. The `ipc` handler runs the invoked
    /// command while holding the plugin store lock, and the event loop takes that lock to
    /// deliver `RunEvent`s, so a command that waits on the main thread — a menu or tray
    /// command, say — deadlocks with it from any other thread. And the objects those
    /// commands create or drop, an `NSStatusItem` for one, are main-thread only.
    ///
    /// `responder` is safe to call from any thread, so a handler that responds
    /// asynchronously from a thread of its own keeps working.
    fn process_request(
      &self,
      request: Option<&mut Request>,
      callback: Option<&mut Callback>,
    ) -> ::std::os::raw::c_int {
      let Some(request) = request else { return 0 };
      let Some(callback) = callback else { return 0 };

      let url = CefString::from(&request.url()).to_string();
      let url = Url::parse(&url).ok();

      if let Some(url) = url {
        let callback = ThreadSafe(callback.clone());
        let response_store = ThreadSafe(self.response.clone());
        let initialization_scripts = self.initialization_scripts.clone();
        let is_main_frame = self.is_main_frame;
        let responder = Box::new(move |response: http::Response<Cow<'static, [u8]>>| {
          let is_html = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .map(|ct| ct.to_lowercase().starts_with("text/html"))
            .unwrap_or(false);

          let (parts, body) = response.into_parts();
          let body_bytes = body.into_owned();
          let body_bytes = if is_html {
            inject_scripts_into_html_body(&body_bytes, &initialization_scripts, is_main_frame)
              .unwrap_or(body_bytes)
          } else {
            body_bytes
          };

          let mut response = http::Response::from_parts(parts, Cursor::new(body_bytes));

          if let Some(csp) = response.headers_mut().get_mut(CONTENT_SECURITY_POLICY) {
            let csp_string = csp.to_str().unwrap_or_default().to_string();
            let new_csp = csp_inject_initialization_scripts_hashes(
              csp_string,
              &initialization_scripts,
              is_main_frame,
            );
            if let Ok(new_csp) = HeaderValue::from_str(&new_csp) {
              *csp = new_csp;
            }
          }

          response_store.into_owned().borrow_mut().replace(response);

          let callback = callback.into_owned();
          callback.cont();
        });

        let label = self.webview_label.clone();
        let handler = self.handler.clone();

        let data = read_request_body(request);
        let mut headers = get_request_headers(request);

        // The renderer can issue an IPC request before its execution context is
        // fully wired to the loader; in that window Chromium sends the request
        // with `Origin: null` even though the document already has a real
        // origin. Repair it from the initiating main frame's URL, which the
        // browser process tracks reliably. Only done when the renderer sent no
        // origin or a literal `null`, so a correct renderer-sent origin always
        // wins.
        if let Some(initiator_origin) = &self.initiator_origin {
          let origin_missing_or_null = headers
            .get(ORIGIN)
            .map(|value| value.as_bytes() == b"null")
            .unwrap_or(true);
          if origin_missing_or_null && let Ok(value) = HeaderValue::from_str(initiator_origin) {
            headers.insert(ORIGIN, value);
          }
        }

        let method_str = CefString::from(&request.method()).to_string();
        let method = http::Method::from_bytes(method_str.as_bytes()).unwrap_or(http::Method::GET);

        (self.main_thread)(Box::new(move || {
          let mut http_request = http::Request::builder()
            .method(method)
            .uri(url.as_str())
            .body(data)
            .unwrap();
          *http_request.headers_mut() = headers;
          // handler is Arc<Box<UriSchemeProtocol>>, so we need to dereference to call it
          (**handler)(&label, http_request, responder);
        }));
        1
      } else {
        0
      }
    }

    fn read(
      &self,
      data_out: *mut u8,
      bytes_to_read: ::std::os::raw::c_int,
      bytes_read: Option<&mut ::std::os::raw::c_int>,
      _callback: Option<&mut ResourceReadCallback>,
    ) -> ::std::os::raw::c_int {
      let Ok(bytes_to_read) = usize::try_from(bytes_to_read) else {
        return 0;
      };
      let data_out = unsafe { std::slice::from_raw_parts_mut(data_out, bytes_to_read) };
      let count = self
        .response
        .borrow_mut()
        .as_mut()
        .and_then(|response| response.body_mut().read(data_out).ok())
        .unwrap_or(0);
      if let Some(bytes_read) = bytes_read {
        let Ok(count) = count.try_into() else {
          return 0;
        };
        *bytes_read = count;
        if count > 0 {
          return 1;
        }
      }
      0
    }

    fn response_headers(
      &self,
      response: Option<&mut Response>,
      response_length: Option<&mut i64>,
      redirect_url: Option<&mut CefString>,
    ) {
      let (Some(response), Some(response_data)) = (response, &*self.response.borrow()) else {
        return;
      };

      response.set_status(response_data.status().as_u16() as i32);
      let mut content_type = None;

      // Set response headers and remember the MIME type for CEF.
      for (name, value) in response_data.headers() {
        let Ok(value) = value.to_str() else {
          continue;
        };

        response.set_header_by_name(Some(&name.as_str().into()), Some(&value.into()), 0);

        if name == CONTENT_TYPE {
          content_type.replace(value.to_string());
        }
      }

      response.set_header_by_name(Some(&"Cache-Control".into()), Some(&"no-store".into()), 1);

      let mime_type = content_type
        .as_ref()
        .and_then(|t| t.split(';').next())
        .map(str::trim)
        .unwrap_or("text/plain");
      response.set_mime_type(Some(&mime_type.into()));

      if let Some(length) = response_length {
        *length = -1;
      }

      if let Some(redirect_url) = redirect_url {
        let _ = std::mem::take(redirect_url);
      }
    }
  }
}

wrap_scheme_handler_factory! {
  pub struct UriSchemeHandlerFactory {
    registry: SchemeRegistry,
    scheme: String,
    main_thread: MainThreadDispatcher,
    // `Cef::frame_hosts_without_ipc`.
    frame_hosts_without_ipc: Arc<Vec<String>>,
  }

  impl SchemeHandlerFactory {
    fn create(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      _scheme_name: Option<&CefString>,
      request: Option<&mut Request>,
    ) -> Option<ResourceHandler> {
      // A frame left without `window.ipc`, or nested in one, reaches no custom scheme but
      // the hosts it is on: Tauri's `ipc` protocol would be the same IPC by another door,
      // and the application's other schemes are no business of its document either. Keyed
      // on the frame CEF names for the request, never on headers the renderer wrote.
      if let (Some(frame), Some(request)) = (frame.as_deref(), request.as_deref())
        && !self.frame_hosts_without_ipc.is_empty()
        && crate::cef_impl::ipc::frame_without_ipc(frame, &self.frame_hosts_without_ipc)
        && !crate::cef_impl::ipc::url_on_hosts(
          &CefString::from(&request.url()).to_string(),
          &self.frame_hosts_without_ipc,
        )
      {
        log::debug!(
          "refused a `{}` request from a frame without `window.ipc`",
          self.scheme
        );
        return Some(refused_from_frame_without_ipc(self.main_thread.clone()));
      }

      let (webview_label, handler, initialization_scripts, ipc_access_policy) = match browser {
        Some(browser) => {
          let id = browser.identifier();

          // get handler from our regsitry based on browser ID and scheme
          self
            .registry
            .lock()
            .unwrap()
            .get(&(id, self.scheme.clone()))
            .cloned()?
        }
        // `browser`/`frame` are null for requests that do not originate from a
        // browser: service worker (main script and update) fetches and
        // CefURLRequest. Never allow `ipc` or non-`tauri` app schemes to borrow
        // an arbitrary live webview's registration and label without a browser/frame.
        None => {
          if self.scheme == "ipc" {
            log::debug!("refused a browserless `ipc` custom-scheme request");
            return Some(refused_from_frame_without_ipc(self.main_thread.clone()));
          }
          if self.scheme != "tauri" {
            return None;
          }
          let (webview_label, handler, _, _) = self
            .registry
            .lock()
            .unwrap()
            .iter()
            .find(|((_, scheme), _)| *scheme == self.scheme)
            .map(|(_, entry)| entry.clone())?;
          (webview_label, handler, Arc::new(Vec::new()), None)
        }
      };

      // Enforce the webview's capability-derived IPC access policy on `ipc://` requests
      // using the browser-owned initiating frame, never trusting request headers alone.
      if self.scheme == "ipc"
        && let Some(policy) = ipc_access_policy.as_deref()
      {
        let allowed = frame.as_deref().is_some_and(|frame| {
          let frame_url = CefString::from(&frame.url()).to_string();
          Url::parse(&frame_url)
            .ok()
            .is_some_and(|parsed| policy.allows_url(&parsed, frame.is_main() != 0))
        });
        if !allowed {
          log::debug!("refused an `ipc` custom-scheme request rejected by IPC access policy");
          return Some(refused_from_frame_without_ipc(self.main_thread.clone()));
        }
      }

      // A subframe navigation is the only case we can positively identify here, so anything
      // else (a null frame included) keeps the main frame behavior of injecting everything.
      let is_main_frame = frame.as_ref().map(|frame| frame.is_main() == 1).unwrap_or(true);

      // Capture the initiating main frame's origin so `process_request` can
      // repair a racy `Origin: null` header. Restricted to the main frame, and
      // for `ipc` only when the main frame's URL is admitted by the IPC access
      // policy (so a sandboxed/untrusted main frame never has `Origin: null` upgraded).
      let initiator_origin = frame
        .filter(|frame| frame.is_main() == 1)
        .map(|frame| CefString::from(&frame.url()).to_string())
        .and_then(|url| Url::parse(&url).ok())
        .filter(|url| {
          self.scheme != "ipc"
            || ipc_access_policy
              .as_deref()
              .is_none_or(|policy| policy.allows_url(url, true))
        })
        .map(|url| url.origin().ascii_serialization())
        .filter(|origin| origin != "null");

      Some(WebResourceHandler::build(WebResourceHandlerArgs {
        webview_label,
        handler,
        initialization_scripts,
        is_main_frame,
        initiator_origin,
        main_thread: self.main_thread.clone(),
        response: Arc::new(RefCell::new(None)),
      }))
    }
  }
}

/// The answer to a custom-scheme request from a frame without `window.ipc` (see
/// `UriSchemeHandlerFactory::create`): `403`, readable by the document so it can tell, and
/// nothing from the application's handler.
fn refused_from_frame_without_ipc(main_thread: MainThreadDispatcher) -> ResourceHandler {
  let handler: Box<UriSchemeProtocolHandler> = Box::new(|_label, _request, responder| {
    let response = http::Response::builder()
      .status(http::StatusCode::FORBIDDEN)
      .header(CONTENT_TYPE, "text/plain")
      .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
      .body(Cow::Borrowed(
        b"refused: this frame has no IPC and reaches no other custom scheme".as_slice(),
      ))
      .expect("a static response");
    responder(response);
  });
  WebResourceHandler::build(WebResourceHandlerArgs {
    webview_label: String::new(),
    handler: Arc::new(handler),
    initialization_scripts: Arc::new(Vec::new()),
    is_main_frame: false,
    initiator_origin: None,
    main_thread,
    response: Arc::new(RefCell::new(None)),
  })
}

struct ThreadSafe<T>(T);

impl<T> ThreadSafe<T> {
  fn into_owned(self) -> T {
    self.0
  }
}

unsafe impl<T> Send for ThreadSafe<T> {}
unsafe impl<T> Sync for ThreadSafe<T> {}

fn read_request_body(request: &mut Request) -> Vec<u8> {
  let mut body = Vec::new();

  if let Some(post_data) = request.post_data() {
    let mut elements = vec![None; post_data.element_count()];
    post_data.elements(Some(&mut elements));
    for element in elements.into_iter().flatten() {
      match element.get_type().as_ref() {
        sys::cef_postdataelement_type_t::PDE_TYPE_BYTES => {
          let size = element.bytes_count();
          if size > 0 {
            let mut buf = vec![0u8; size];
            // Copy bytes into our buffer
            let copied = element.bytes(size, buf.as_mut_ptr());
            // Safety: CEF promises it wrote `copied` bytes into buf
            unsafe {
              buf.set_len(copied);
            }
            body.extend(buf);
          }
        }
        sys::cef_postdataelement_type_t::PDE_TYPE_FILE => {
          // Read file from disk
          let file_path = CefString::from(&element.file()).to_string();
          if let Ok(mut file) = std::fs::File::open(&file_path) {
            use std::io::Read;
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() {
              body.extend(buf);
            }
          }
        }
        _ => {}
      }
    }
  }

  body
}

fn get_request_headers(request: &mut Request) -> HeaderMap {
  let mut headers = HeaderMap::new();

  let mut map = CefStringMultimap::new();

  request.header_map(Some(&mut map));

  // Iterate through all entries
  for (name, value) in map {
    for v in value {
      headers.append(
        HeaderName::from_bytes(name.as_bytes()).unwrap(),
        HeaderValue::from_str(&v).unwrap(),
      );
    }
  }

  headers
}

// ==== Renderer termination boundary ====

fn termination_reason(
  status: TerminationStatus,
) -> tauri_runtime::webview::WebContentProcessTerminationReason {
  use tauri_runtime::webview::WebContentProcessTerminationReason as Reason;
  match status {
    TerminationStatus::ABNORMAL_TERMINATION => Reason::Abnormal,
    TerminationStatus::PROCESS_WAS_KILLED => Reason::Killed,
    TerminationStatus::PROCESS_CRASHED => Reason::Crashed,
    TerminationStatus::PROCESS_OOM => Reason::OutOfMemory,
    TerminationStatus::LAUNCH_FAILED => Reason::LaunchFailed,
    TerminationStatus::INTEGRITY_FAILURE => Reason::IntegrityFailure,
    _ => Reason::Unknown,
  }
}

#[cfg(test)]
mod termination_tests {
  use super::*;
  use tauri_runtime::webview::WebContentProcessTerminationReason as Reason;

  #[test]
  fn preserves_every_cef_termination_reason() {
    for (status, expected) in [
      (TerminationStatus::ABNORMAL_TERMINATION, Reason::Abnormal),
      (TerminationStatus::PROCESS_WAS_KILLED, Reason::Killed),
      (TerminationStatus::PROCESS_CRASHED, Reason::Crashed),
      (TerminationStatus::PROCESS_OOM, Reason::OutOfMemory),
      (TerminationStatus::LAUNCH_FAILED, Reason::LaunchFailed),
      (
        TerminationStatus::INTEGRITY_FAILURE,
        Reason::IntegrityFailure,
      ),
      (TerminationStatus::NUM_VALUES, Reason::Unknown),
    ] {
      assert_eq!(termination_reason(status), expected);
    }
  }
}

#[cfg(test)]
mod app_protocol_tests {
  use super::*;

  #[test]
  fn blocks_app_protocol_hosts_without_blocking_loopback_servers() {
    let names = ["ipc".to_owned(), "gups-plugin".to_owned()];
    for url in [
      "http://ipc.localhost/shell_info",
      "https://GUPS-plugin.localhost.:4200/",
      "gups-plugin://localhost/path",
    ] {
      assert!(is_app_protocol_url(&url.parse().unwrap(), &names), "{url}");
    }
    for url in [
      "http://localhost:4200",
      "http://127.0.0.1:4200",
      "http://ipc.localhost.example/",
    ] {
      assert!(!is_app_protocol_url(&url.parse().unwrap(), &names), "{url}");
    }
  }
}

#[cfg(test)]
mod proxy_auth_tests {
  use super::*;

  #[test]
  fn only_proxy_challenges_pass_credentials_to_cef() {
    crate::initialize_cef_test_api();
    let credentials = std::cell::RefCell::new(None);
    let host = CefString::from("127.0.0.1");
    let realm = CefString::from("test");
    let scheme = CefString::from("basic");
    let handler = |host: &str, port, realm: &str, scheme: &str| {
      assert_eq!(
        (host, port, realm, scheme),
        ("127.0.0.1", 8080, "test", "basic")
      );
      Some(("test-user".to_owned(), "test-password".to_owned()))
    };
    let answer = |is_proxy, port| {
      answer_proxy_challenge(
        Some(&handler),
        is_proxy,
        Some(&host),
        port,
        Some(&realm),
        Some(&scheme),
        |username, password| {
          *credentials.borrow_mut() = Some((username.to_owned(), password.to_owned()));
        },
      )
    };

    assert_eq!(answer(false, 9), 0);
    assert_eq!(answer(true, -1), 0);
    assert_eq!(*credentials.borrow(), None);
    assert_eq!(answer(true, 8080), 1);
    assert_eq!(
      *credentials.borrow(),
      Some(("test-user".into(), "test-password".into())),
    );
  }
}

#[cfg(test)]
mod before_resource_load_tests {
  use super::*;
  use crate::BeforeResourceLoad;

  #[test]
  fn guarded_requests_continue_redirect_or_cancel() {
    crate::initialize_cef_test_api();
    let url = "http://localhost/file";
    let redirect = "http://localhost/redirect";
    for (decision, status, target) in [
      (
        BeforeResourceLoad::Continue,
        sys::cef_return_value_t::RV_CONTINUE,
        url,
      ),
      (
        BeforeResourceLoad::Redirect(redirect.parse().unwrap()),
        sys::cef_return_value_t::RV_CONTINUE,
        redirect,
      ),
      (
        BeforeResourceLoad::Cancel,
        sys::cef_return_value_t::RV_CANCEL,
        url,
      ),
    ] {
      let guard = guarded_resource_handler(
        Some(Arc::new(move |input| {
          assert_eq!(input.as_str(), url);
          decision.clone()
        })),
        None,
      )
      .unwrap();
      let mut request = cef::request_create().unwrap();
      request.set_url(Some(&CefString::from(url)));
      assert_eq!(
        guard.on_before_resource_load(None, None, Some(&mut request), None),
        status.into()
      );
      assert_eq!(CefString::from(&request.url()).to_string(), target);
    }
  }
}
