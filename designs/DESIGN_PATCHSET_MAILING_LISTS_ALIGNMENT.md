# Design Document: Aligning Patchset Mailing Lists vs Bug Subsystems

## 1. Background & Problem Statement

In Sashiko's ingestion pipeline, incoming patchset emails are matched against recipient domains (`@vger.kernel.org`, `@lists.linux.dev`, `@lists.infradead.org`, `@kvack.org`, or regex rules in `Settings.toml`). Historically, these recipient matches were stored in a table named `subsystems` and exposed to the frontend as "subsystems" (e.g. `LKML`, `netdev`, `bpf`).

In reality:
- For **patchsets**, these tags are **mailing lists**, not kernel subsystems.
- For **bugs** (discovered defects and pre-existing bugs), Sashiko's engine maps touched source code paths to actual Linux kernel **subsystems** via the `MAINTAINERS` file index (`fs`, `mm`, `net`, etc.).

Currently, both the patchset frontend and APIs display and return mailing list tags under the label and field name `subsystems`. This creates conceptual confusion between mailing list delivery channels and kernel source subsystems.

## 2. Goals & Non-Goals

### Goals
- Update the **frontend** so patchsets clearly display **"Mailing Lists"** instead of "Subsystems".
- Update the **APIs** (`/api/patchset`, `/api/patch`, `/api/patchsets`) to return `mailing_lists` for patchsets.
- Maintain full backward compatibility for API consumers by providing both `mailing_lists` and `subsystems` on patchset payloads.
- Preserve the **"Subsystems"** terminology for bugs (bug tracker, bug detail view, bug search filters, and the "Discovered Linux Kernel Bugs" section in patchset review) where it genuinely represents kernel subsystems.

### Non-Goals
- Renaming the underlying SQLite tables (`patchsets_subsystems`, `subsystems`) in this change. Leaving the database schema unchanged avoids complex migrations and guarantees zero disruption to existing databases.

---

## 3. Detailed Proposed Changes

### 3.1 API & Data Models (`src/db.rs`, `src/api.rs`)

1. **`PatchsetRow` Struct (`src/db.rs`)**:
   Add `mailing_lists: Vec<String>` alongside `subsystems: Vec<String>`:
   ```rust
   #[derive(Debug, Serialize, Deserialize, Clone)]
   pub struct PatchsetRow {
       ...
       pub mailing_lists: Vec<String>,
       pub subsystems: Vec<String>,
       ...
   }
   ```
   When constructing `PatchsetRow` (in `get_patchsets`, `get_pending_patchsets`), populate both `mailing_lists` and `subsystems` with the extracted list names.

2. **`/api/patchset` (`get_patchset_summary` in `src/db.rs`)**:
   Include `"mailing_lists"` in the returned JSON object:
   ```rust
   "subsystems": subsystems.clone(),
   "mailing_lists": subsystems,
   ```

3. **`/api/patch` (`get_patchset_details` in `src/db.rs`)**:
   Include `"mailing_lists"` in the returned JSON object:
   ```rust
   "subsystems": subsystems.clone(),
   "mailing_lists": subsystems,
   ```

### 3.2 Frontend (`static/index.html`)

1. **Patchset Detail View (`renderPatchsetView`)**:
   - Update the metadata section label from `Subsystems:` to `Mailing Lists:`.
   - Update variable to `mailingListsHtml`, reading `data.mailing_lists || data.subsystems`.
   ```javascript
   const mailingLists = data.mailing_lists || data.subsystems;
   const mailingListsHtml = mailingLists?.length > 0
       ? mailingLists.map(s => `<span class="tag">${escapeHtml(s)}</span>`).join('')
       : '-';
   ...
   <div class="kv"><div class="label">Mailing Lists:</div><div>${mailingListsHtml}</div></div>
   ```

2. **Patchset List Table (`renderList`)**:
   - Update the patchset item row to consume `p.mailing_lists || p.subsystems`:
   ```javascript
   const lists = p.mailing_lists || p.subsystems;
   let tagsHtml = '';
   if (lists && Array.isArray(lists) && lists.length > 0) {
       tagsHtml = '<div style="margin-top:2px">' + lists.map(s => `<span class="tag">${escapeHtml(s)}</span>`).join('') + '</div>';
   }
   ```

3. **Bugs Views (Unchanged)**:
   - The bug filter dropdown (`#subsystemDropdown`), bug search parameters (`/api/bugs?subsystems=...`), bug cards (`b.subsystems`), and the "Discovered Linux Kernel Bugs" section remain labeled as **Subsystems** because they accurately reflect kernel subsystems.

---

## 4. Verification & Testing

1. **Unit & Integration Tests**:
   - Ensure all existing database and API tests compile and pass with `make check-pr`.
   - Add unit test verifying that `get_patchset_summary` and `get_patchset_details` return `mailing_lists` in their JSON payloads.
2. **Frontend Verification**:
   - Verify that the patchset list view displays mailing list tags under patchset subjects.
   - Verify that the patchset detail view displays the "Mailing Lists:" label and tags.
   - Verify that the bug list view and bug detail views still display "Subsystems".
