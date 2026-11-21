// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use cef::*;

use crate::{FindResult, FindResultHandler, FrameNavigationState};

wrap_find_handler! {
  pub(super) struct TauriCefFindHandler {
    handler: Arc<FindResultHandler>,
    frame_navigation_state: FrameNavigationState,
  }

  impl FindHandler {
    fn on_find_result(
      &self,
      browser: Option<&mut Browser>,
      identifier: i32,
      count: i32,
      selection_rect: Option<&Rect>,
      active_match_ordinal: i32,
      final_update: i32,
    ) {
      let browser_id = browser.map(|browser| browser.identifier());
      let result = FindResult {
        identifier,
        count,
        selection_rect: selection_rect.cloned(),
        active_match_ordinal,
        final_update: final_update != 0,
      };
      emit_result(self.handler.as_ref(), &self.frame_navigation_state, browser_id, result);
    }
  }
}

fn emit_result(
  handler: &FindResultHandler,
  state: &FrameNavigationState,
  browser_id: Option<i32>,
  result: FindResult,
) {
  if browser_id.is_some_and(|id| state.has_browser_id(id)) {
    handler(result);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_delivers_results_for_the_observed_browser() {
    let state = FrameNavigationState::new();
    state.on_frame_event(&crate::FrameEvent {
      browser_id: 7,
      frame_id: "main".into(),
      is_main: true,
      kind: crate::FrameEventKind::Created,
    });
    let results = Arc::new(std::sync::Mutex::new(Vec::<FindResult>::new()));
    let observed = results.clone();
    let handler = move |result| observed.lock().unwrap().push(result);
    let result = FindResult {
      identifier: 3,
      count: 8,
      selection_rect: None,
      active_match_ordinal: 2,
      final_update: true,
    };
    emit_result(&handler, &state, Some(9), result.clone());
    emit_result(&handler, &state, Some(7), result);
    let results = results.lock().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].identifier, 3);
    assert_eq!(results[0].count, 8);
    assert_eq!(results[0].active_match_ordinal, 2);
    assert!(results[0].final_update);
  }
}
