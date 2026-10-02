# Dashboard screenshots
Hand-made `state.json` is the only API data; no ledger or daemon is used.
With the web dependencies already available, run `web/node_modules/.bin/tsc --ignoreConfig --noEmit --strict --resolveJsonModule --esModuleInterop --moduleResolution bundler --module esnext --target es2022 docs/screenshots/check.ts` from the repository root.
Build with `cd web && ./node_modules/.bin/vite build`; run `python3 docs/screenshots/serve.py` from the root in another terminal.
From the root, run `sh docs/screenshots/capture.sh`; it uses a fresh Chrome profile for each theme and writes both PNGs.
