# Design: Bug Page Query and Payload Optimization

## Status
Approved

## Context & Problem Statement
Loading the bug page (both the bug list view `/api/bugs` and individual bug details `/api/bug`) suffers from high latency (2 to 3+ seconds) and excessive bandwidth usage.

Profiling against the live service revealed two major root causes:
1. **Unneeded Gzip Decompression of Massive LLM Interaction Logs:**
   The `bug_enrichments` table contains compressed raw LLM execution logs in the `logs` column (often 200 KB – 500 KB compressed per bug, expanding to multiple megabytes of text).
   Whenever `Database::get_bug_enrichments` is called, it unconditionally queries and decompresses the `logs` column using `flate2::write::GzDecoder`.
   For the bug list view (`/api/bugs`), which fetches 50 bugs with ~8 enrichments each (~400 rows total), the server decompresses over 20 MB of Gzip data synchronously on Tokio worker threads.
   Furthermore, `serde_json::to_value(bug)` serializes all decompressed logs into the JSON response, producing a **23.6 MB payload** for a single 50-item list request.
   Over **89%** of the payload consists of raw logs, and **99.9%** of the data is completely unused by the frontend table view (which only renders `problem`, `severity`, `status`, `subsystems`, and `date`).
2. **Sequential $N+1$ Database Queries:**
   In `Database::list_bugs`, after fetching the 50 bugs, it loops sequentially over each bug executing:
   - 1 query for `get_subsystems_for_bug`
   - 1 query for `get_bug_enrichments`
   This produces **101 separate database round-trip queries** per page load.

Individual bug detail requests (`/api/bug?id=...`) suffer similarly: each bug response is ~1.3 MB, with over 90% comprising logs that `renderBugView` never renders (raw logs are only shown on the dedicated `#/bug/<id>/log` view via `/api/bug/logs`).

## Design Details

### 1. Decouple Logs from Standard Enrichment Queries
- Update `Database::get_bug_enrichments` to select `NULL as logs`.
- Because `parse_bug_enrichment_row` parses `NULL` as `None` and `BugEnrichment` specifies `#[serde(skip_serializing_if = "Option::is_none")]`, `logs` is omitted from serialization and zero Gzip decompression occurs during regular bug fetching.
- Update `Database::get_bug_logs` to directly query `tool, logs` from `bug_enrichments` where `logs IS NOT NULL` on demand, isolating the decompression cost exclusively to the `/api/bug/logs` endpoint.

### 2. Batch Subsystems and Enrichments in `Database::list_bugs`
- In `Database::list_bugs`, replace the sequential per-row queries with two batched queries using `IN (...)`:
  1. `SELECT bug_id, subsystem FROM bugs_subsystems WHERE bug_id IN (...) ORDER BY subsystem ASC`
  2. `SELECT id, bug_id, kind, tool, model, author, created_at, content, data_json, tokens_in, tokens_out, tokens_cached, NULL as logs FROM bug_enrichments WHERE bug_id IN (...) ORDER BY created_at ASC, id ASC`
- Map results in memory using hash tables (`HashMap<i64, Vec<_>>`) and assign them to each `Bug`.
- This reduces database round-trips from **101 queries** down to **3 queries**.

### 3. Omit `enrichments` from `/api/bugs` List Responses
- The frontend list view (`renderListView`) derives computed summary fields (`problem`, `severity`, `status`, `subsystems`, `date`) and does not render the `enrichments` array.
- In `src/api.rs:list_bugs`, strip the `enrichments` field from each serialized bug object in the response:
  ```rust
  val.as_object_mut().map(|obj| obj.remove("enrichments"));
  ```
- This trims an additional ~330 KB of unused intermediate content and JSON blobs, shrinking the list response from **23.6 MB** to **< 50 KB**.

## Verification Plan
1. Run `cargo test` and `make check-pr` to ensure all existing API tests and invariants pass.
2. Measure `/api/bugs` and `/api/bug` against the live local database:
   - Verify TTFB drops from ~2.1s to < 25ms.
   - Verify `/api/bugs` payload size drops from 23.6 MB to < 50 KB.
   - Verify `/api/bug/logs` continues to return complete logs.
   - Verify the web UI renders bug list and bug details correctly.
