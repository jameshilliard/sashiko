# Design: Frontend Token Expiration and Rejection Handling

## Status
Approved

## Context & Motivation
Sashiko's web frontend authenticates via stateless JWT session tokens stored in the browser's local storage (`sashiko_session_token`). The server enforces access control and session lifetimes (e.g. 24-hour expiration for session tokens) and returns HTTP 401 Unauthorized for expired/invalid tokens and HTTP 403 Forbidden for blocklisted or unauthorized entities.

Prior to this improvement:
1. `safeStorage` implemented `getItem` and `setItem`, but lacked `removeItem`. Calling `safeStorage.removeItem` inside `logoutAuth()` triggered a runtime `TypeError`, failing to delete stored tokens.
2. The frontend did not inspect JWT expiration claims (`exp`) client-side. An expired token remained in storage across sessions, showing the user as logged in and repeatedly issuing failing API calls with expired credentials.
3. The global `window.fetch` wrapper did not intercept HTTP 401 Unauthorized responses from the server, leaving dead session tokens in place after server rejection.
4. Token refresh failures and sign-in link rejections lacked clean, unified session cleanup and feedback.

## Design Details

### 1. Fix `safeStorage.removeItem`
Add `removeItem(key)` to `safeStorage` with exception handling for disabled or restricted `localStorage`.

### 2. Client-Side Token Expiration Detection
Implement `parseJwtPayload(token)` and `isTokenExpired(token)`:
- Base64URL-decode and parse the JWT payload.
- Compare `Date.now() / 1000` against the `exp` claim, using a 5-second grace window to protect against clock drift and race conditions.
- When `getAuthToken()` is accessed, if the token is expired, immediately purge the stored session and return `null`.

### 3. Server Rejection Interception in `window.fetch`
When an API request sends an `Authorization` header with a session token:
- If the server responds with `HTTP 401 Unauthorized`, the token is invalid or expired. Intercept this response and invoke `logoutAuth()`.
- If `/api/auth/refresh` responds with `HTTP 401` or `HTTP 403 Forbidden` (e.g., user blocklisted or revoked), invoke `logoutAuth()`.
- Guard `logoutAuth()` against re-entrancy using an `isLoggingOut` lock.

### 4. Background and Passive Expiration
- Run an expiration check interval (every 30 seconds) so idle tabs automatically transition to the unauthenticated "Guest" state once the token expires.
- Update `refreshPageData()` to use the application's actual view router `router()` instead of the non-existent `render()`.

### 5. Sign-in and Verification Feedback
- In `fetchNewSessionToken()`, distinguish between expired sign-in links (401) and blocked/forbidden accounts (403), clearing stale session state upon verification failure.
