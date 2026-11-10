# Web UI browser tests

Playwright tests that drive the bridge's web UI (`../ui/`) the way a person
on site does, against `x9-mock`. The run starts `x9-mock` and `x9-bridge` on
spare localhost ports (as `../tests/e2e.py` does), with a temporary tokens
file, data directory and recipes, and stops both with SIGINT at the end.
Nothing leaves localhost.

## Run

```sh
cargo build -p x9-bridge -p x9-mock   # the UI is built into the bridge: rebuild after changing it
cd crates/x9-bridge/ui-tests
npm ci
PW_CHANNEL=chrome npm test           # the installed Google Chrome, no download
```

Without `PW_CHANNEL`, Playwright's own Chromium is used; install it once with
`npx playwright install --with-deps chromium` (what CI does).

- `X9_BIN` — directory with the binaries, default `../../../target/debug`
  (e.g. `X9_BIN=../../../target/release`)
- `X9_UI_LOGS=1` — show the mock's and bridge's output

## Coverage

Each page records V8 JS coverage (Chromium only), taken again before each
reload since V8 drops a page's counts when it unloads. After the run the
ranges are merged across every test, a table of bytes run and functions entered for
`app.js` and `viewer.js` is printed, and `coverage/ui-coverage.json` is
written, with the functions never entered listed by line.

## What the mock can't give

The mock serves a synthetic preview picture but no scan file, so anything that
decodes points (aim, export, measure, the 3D viewer, registration, the
next-spot planner and its overlay, asking a question) is tested in its error
state. With real scans (`X9_MOCK_FIXTURES`) those paths are covered by
`../tests/e2e.py`, not here.

Tests marked `test.fixme` show UI bugs found while writing these; each says
why above it. Run one with `test.only` to see it fail.
