// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use cef::*;

wrap_load_handler! {
  pub struct TauriCefLoadHandler {
    on_page_load_handler: Option<Arc<tauri_runtime::webview::OnPageLoadHandler>>,
    frame_event_handler: Option<Arc<crate::FrameEventHandler>>,
  }

  impl LoadHandler {
    fn on_loading_state_change(
      &self,
      browser: Option<&mut Browser>,
      is_loading: ::std::os::raw::c_int,
      can_go_back: ::std::os::raw::c_int,
      can_go_forward: ::std::os::raw::c_int,
    ) {
      if let Some(browser) = browser {
        let mut frame = browser.main_frame();
        crate::frame::emit_frame_event(
          &self.frame_event_handler,
          Some(browser),
          frame.as_mut(),
          loading_state(is_loading, can_go_back, can_go_forward),
        );
      }
    }

    fn on_load_start(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      _transition_type: TransitionType,
    ) {
      let Some(frame) = frame else {
        return;
      };
      let url = cef::CefString::from(&frame.url()).to_string();
      if let Ok(url) = url::Url::parse(&url) {
        let is_main = frame.is_main() != 0;
        crate::frame::emit_frame_event(
          &self.frame_event_handler,
          browser,
          Some(frame),
          crate::FrameEventKind::DocumentCommitted { url: url.clone() },
        );
        if is_main && let Some(handler) = &self.on_page_load_handler {
          handler(url, tauri_runtime::webview::PageLoadEvent::Started);
        }
      }
    }

    fn on_load_error(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      error_code: Errorcode,
      error_text: Option<&CefString>,
      failed_url: Option<&CefString>,
    ) {
      if let Some(failed_url) = failed_url
        && let Ok(url) = url::Url::parse(&failed_url.to_string())
      {
        crate::frame::emit_frame_event(
          &self.frame_event_handler,
          browser,
          frame,
          navigation_failure(url, error_code, error_text),
        );
      }
    }

    fn on_load_end(
      &self,
      _browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      _http_status_code: ::std::os::raw::c_int,
    ) {
      let Some(handler) = &self.on_page_load_handler else {
        return;
      };
      let Some(frame) = frame else {
        return;
      };

      if frame.is_main() == 0 {
        return;
      }

      let url = cef::CefString::from(&frame.url()).to_string();
      if let Ok(url) = url::Url::parse(&url) {
        handler(url, tauri_runtime::webview::PageLoadEvent::Finished);
      }
    }
  }
}

fn loading_state(is_loading: i32, can_go_back: i32, can_go_forward: i32) -> crate::FrameEventKind {
  crate::FrameEventKind::LoadingStateChanged {
    is_loading: is_loading != 0,
    can_go_back: can_go_back != 0,
    can_go_forward: can_go_forward != 0,
  }
}

fn navigation_failure(
  url: url::Url,
  error_code: Errorcode,
  error_text: Option<&CefString>,
) -> crate::FrameEventKind {
  crate::FrameEventKind::NavigationFailed {
    url,
    error_code: *error_code.as_ref() as i32,
    error_text: error_text.map(ToString::to_string).unwrap_or_default(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::FrameEventKind;

  #[test]
  fn loading_state_preserves_both_history_flags() {
    assert!(matches!(
      loading_state(0, 1, 0),
      FrameEventKind::LoadingStateChanged {
        is_loading: false,
        can_go_back: true,
        can_go_forward: false
      }
    ));
    assert!(matches!(
      loading_state(1, 0, 1),
      FrameEventKind::LoadingStateChanged {
        is_loading: true,
        can_go_back: false,
        can_go_forward: true
      }
    ));
  }

  #[test]
  fn navigation_failure_preserves_chromium_error() {
    crate::initialize_cef_test_api();
    let url = url::Url::parse("https://example.test/").unwrap();
    let event = navigation_failure(url.clone(), Errorcode::ABORTED, Some(&"ERR_ABORTED".into()));
    assert!(matches!(event, FrameEventKind::NavigationFailed {
      url: failed_url,
      error_code: -3,
      error_text,
    } if failed_url == url && error_text == "ERR_ABORTED"));
  }
}
