# KalaDoc UI

A React single-page application for the KalaDoc HTTP API, served by
`kaladoc serve`. This directory (`ui/`) lives inside the KalaDoc repository
and talks to the backend over REST — it is not a build step for the Rust
crate.

## Stack

- Vite 6 dev server / build
- React 19 + TypeScript
- Tailwind CSS v4 (via `@tailwindcss/vite`)
- TanStack Query v5 for server state
- React Router v7 for routing

## Prerequisites

- Node.js 20 or newer (the project is developed on Node 22)
- A running KalaDoc backend: `kaladoc serve` (binds `api.addr`, default
  `http://127.0.0.1:8080`)
- A bearer token. There is **no HTTP login endpoint** — obtain a token one of
  two ways:
  - **Session token:** run `kaladoc auth login` and use the issued token.
  - **Static API key:** configure `KALADOC_API_KEYS` or `api.api_keys` in
    `kaladoc.toml` as `name:key` pairs, and use the key value as the token.

The token is entered on the UI's Connect page (or the Settings page) and
stored in `localStorage` under the key `kaladoc.settings`. It is only sent to
the configured API origin; clear it via Settings when done.

## Development

```bash
cd ui
npm install
npm run dev        # Vite dev server on http://localhost:5173
```

Other scripts:

```bash
npm run build      # tsc typecheck + production build into dist/
npm run preview    # serve the production build locally
```

On first visit the app redirects to `/connect`; enter the API URL and token,
then it navigates to `/documents`.

## Configuration

| Setting | Source | Notes |
|---|---|---|
| `VITE_API_URL` | env var at build/dev time | Default API URL when nothing is stored |
| API URL / token | `localStorage` (`kaladoc.settings`) | Runtime override; wins over `VITE_API_URL` default |

`src/lib/settings.ts` loads `{apiUrl, token}` from `localStorage`, falling
back to `VITE_API_URL` or `http://localhost:8080`. The Connect and Settings
pages write both fields back to the same key.

## API notes

- All endpoints except `/health` and `/openapi.json` require
  `Authorization: Bearer <token>`. The client throws before the request if no
  token is configured.
- Error responses use the envelope
  `{ "error": { "code", "message", "request_id", "retryable" } }`, surfaced
  as `ApiError` in `src/lib/api.ts`.
- The full route list is documented in `src/crates/api/src/lib.rs` of the
  backend; `GET /openapi.json` serves a generated OpenAPI 3.x spec.

## Structure

```
src/
  main.tsx              entry point (QueryClient + Router providers)
  App.tsx               routes: /connect (public), /documents, /documents/:id,
                        /jobs, /settings (all behind RequireAuth)
  components/
    Layout.tsx          app shell / nav
    StatusBadge.tsx     shared status indicator
  pages/
    Connect.tsx         API URL + token entry; validates via /engines
    ...                 Documents, DocumentDetail, Jobs, Settings (in progress)
  lib/
    api.ts              fetch client, ApiError, get/post/patch/del/download
    settings.ts         localStorage-backed ApiSettings
    types.ts            API response/request types
    format.ts           display formatting helpers
  index.css             Tailwind v4 entry
```

Routes are guarded by `RequireAuth` (`src/App.tsx`): without a configured
token every path redirects to `/connect`.

## Summary

This README documents the new `ui/` React SPA — a Vite + React 19 +
Tailwind v4 + TanStack Query v5 + React Router v7 frontend that connects to
a `kaladoc serve` instance. Setup requires Node 20+ and a bearer token from
`kaladoc auth login` or `api.api_keys`; the token is pasted into the Connect
page (no HTTP login exists) and persisted in `localStorage` under
`kaladoc.settings`. `VITE_API_URL` sets the default backend URL, which the
Settings page can override at runtime. Development is `npm install &&
npm run dev` on port 5173; production builds go to `dist/`. The API surface
is bearer-token authenticated with a standard error envelope, and the
backend's route list lives in `src/crates/api/src/lib.rs` plus the public
`/openapi.json` spec.
