// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import { expect, test } from './fixtures.js'

test.describe('Webview::eval_with_callback', () => {
  test('a run of evaluations is answered in order and the app survives it', async ({
    main
  }) => {
    // Each evaluation registers a one-shot DevTools observer that unregisters
    // itself once its result is in; this used to crash on the first result.
    const count = 200
    const results = await main.evaluate(
      (count) =>
        window.__TAURI__.core.invoke<string[]>('eval_with_callback_sequence', {
          count
        }),
      count
    )
    expect(results).toEqual(
      Array.from({ length: count }, (_, index) => String(index * 2))
    )

    // The observers are unregistered on later tasks; the browser keeps running.
    await expect
      .poll(() => main.evaluate(() => document.readyState))
      .toBe('complete')
  })
})
