// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::db::{Database, StoredPrerequisiteSeries};
use crate::mbox::{LoreMboxClient, split_mbox};
use crate::patch::{Patch, PatchsetMetadata, authors_match, parse_email};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::info;

const MAX_MBOX_MESSAGES: usize = 500;
const MAX_PREREQUISITE_PATCH_IDS: usize = 128;
const MAX_LORE_OPERATIONS: usize = 8;
const MAX_BASED_ON_DEPTH: usize = 8;
const MAX_CONCURRENT_GIT_PATCH_IDS: usize = 8;
const GIT_PATCH_ID_TIMEOUT: Duration = Duration::from_secs(30);

static GIT_PATCH_ID_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrerequisitePatch {
    pub(crate) git_patch_id: String,
    pub(crate) message_id: String,
    pub(crate) subject: String,
    pub(crate) author: String,
    pub(crate) date: i64,
    pub(crate) diff: String,
}

#[derive(Debug, PartialEq, Eq)]
enum PrerequisiteMetadata {
    PatchIds(Vec<String>),
    BasedOn(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PrerequisiteSeries {
    local_patchset_id: Option<i64>,
    message_id: String,
    body: String,
    patches: Vec<PrerequisitePatch>,
}

fn parse_based_on_message_id(body: &str) -> Result<Option<String>> {
    let mut based_on = None;

    for line in body.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("based-on") {
            continue;
        }

        let value = value.trim();
        if value.is_empty() {
            bail!("Based-on metadata has an empty message ID");
        }
        let has_open = value.starts_with('<');
        let has_close = value.ends_with('>');
        if has_open != has_close {
            bail!("Based-on metadata has unmatched angle brackets");
        }
        let message_id = if has_open {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if message_id.is_empty() {
            bail!("Based-on metadata has an empty message ID");
        }
        if message_id.len() > 998 {
            bail!("Based-on message ID exceeds 998 bytes");
        }
        if !message_id.is_ascii()
            || message_id
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || message_id.contains(['<', '>'])
        {
            bail!("Based-on metadata contains an invalid message ID");
        }
        let Some((local, domain)) = message_id.split_once('@') else {
            bail!("Based-on metadata does not contain a message ID");
        };
        if local.is_empty() || domain.is_empty() {
            bail!("Based-on metadata contains an invalid message ID");
        }

        match based_on.as_deref() {
            Some(existing) if existing != message_id => {
                bail!("multiple distinct Based-on message IDs are not supported");
            }
            Some(_) => {}
            None => based_on = Some(message_id.to_string()),
        }
    }

    Ok(based_on)
}

fn parse_prerequisite_metadata(body: &str) -> Result<Option<PrerequisiteMetadata>> {
    let patch_ids = parse_prerequisite_patch_ids(body)?;
    if !patch_ids.is_empty() {
        // Git's explicit patch list and order take precedence over Based-on.
        return Ok(Some(PrerequisiteMetadata::PatchIds(patch_ids)));
    }
    Ok(parse_based_on_message_id(body)?.map(PrerequisiteMetadata::BasedOn))
}

pub(crate) fn parse_prerequisite_patch_ids(body: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();

    for line in body.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("prerequisite-patch-id") {
            continue;
        }

        let id = value.trim();
        if id.len() != 40 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        let id = id.to_ascii_lowercase();
        if seen.insert(id.clone()) {
            if ids.len() == MAX_PREREQUISITE_PATCH_IDS {
                bail!(
                    "b4 metadata contains more than {MAX_PREREQUISITE_PATCH_IDS} unique prerequisite patch IDs"
                );
            }
            ids.push(id);
        }
    }

    Ok(ids)
}

async fn run_git_patch_id_command(
    mut command: tokio::process::Command,
    input: &[u8],
    time_limit: Duration,
) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start git patch-id")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("git patch-id stdin was not piped"))?;

    let operation = async move {
        let write_stdin = async move {
            let result = stdin.write_all(input).await;
            drop(stdin);
            result
        };
        let wait_for_output = async move {
            child
                .wait_with_output()
                .await
                .context("failed to wait for git patch-id")
        };

        let (write_result, output_result) = tokio::join!(write_stdin, wait_for_output);
        let output = output_result?;
        if let Err(error) = write_result
            && (error.kind() != std::io::ErrorKind::BrokenPipe || output.status.success())
        {
            return Err(error).context("failed to write patch to git patch-id");
        }
        Ok::<_, anyhow::Error>(output)
    };

    tokio::time::timeout(time_limit, operation)
        .await
        .map_err(|_| anyhow!("git patch-id timed out after {time_limit:?}"))?
}

/// Calculates the stable Git patch ID for one email patch body.
pub async fn calculate_git_patch_id(diff: &str) -> Result<Option<String>> {
    if diff.trim().is_empty() {
        return Ok(None);
    }

    let _permit = GIT_PATCH_ID_SEMAPHORE
        .get_or_init(|| Semaphore::new(MAX_CONCURRENT_GIT_PATCH_IDS))
        .acquire()
        .await
        .map_err(|_| anyhow!("git patch-id concurrency limiter closed"))?;

    let mut command = crate::git_cmd::detached_async();
    command.args(["patch-id", "--stable"]);
    let output = run_git_patch_id_command(command, diff.as_bytes(), GIT_PATCH_ID_TIMEOUT).await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let Some(id) = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
    else {
        return Ok(None);
    };

    if id.len() == 40 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(id))
    } else {
        Err(anyhow!("git patch-id returned an invalid patch ID"))
    }
}

async fn calculate_optional_git_patch_id(diff: Option<&str>) -> Result<Option<String>> {
    match diff {
        Some(diff) => calculate_git_patch_id(diff).await,
        None => Ok(None),
    }
}

/// Calculates stable IDs for a batch while preserving its input order.
///
/// Empty slots let callers align results with records that have no patch.
/// At most eight subprocesses run at once across the process.
pub async fn calculate_git_patch_id_batch(diffs: Vec<Option<&str>>) -> Vec<Result<Option<String>>> {
    let calculations = diffs.into_iter().map(calculate_optional_git_patch_id);
    futures::stream::iter(calculations)
        .buffered(MAX_CONCURRENT_GIT_PATCH_IDS)
        .collect()
        .await
}

async fn parse_mbox_messages(raw: Vec<u8>) -> Result<Vec<(PatchsetMetadata, Option<Patch>)>> {
    tokio::task::spawn_blocking(move || {
        let messages = split_mbox(&raw);
        if messages.len() > MAX_MBOX_MESSAGES {
            return Err(anyhow!(
                "lore mbox contains {} messages, exceeding the limit of {}",
                messages.len(),
                MAX_MBOX_MESSAGES
            ));
        }

        Ok::<_, anyhow::Error>(
            messages
                .into_iter()
                .filter_map(|message| parse_email(&message).ok())
                .collect::<Vec<_>>(),
        )
    })
    .await
    .context("lore mbox parsing task failed")?
}

fn clean_message_id(message_id: &str) -> &str {
    message_id.trim().trim_matches(['<', '>'])
}

fn metadata_descends_from(metadata: &PatchsetMetadata, ancestors: &HashSet<String>) -> bool {
    metadata
        .in_reply_to
        .as_deref()
        .is_some_and(|parent| ancestors.contains(clean_message_id(parent)))
        || metadata
            .references
            .iter()
            .any(|reference| ancestors.contains(clean_message_id(reference)))
}

fn expand_descendants(
    parsed: &[(PatchsetMetadata, Option<Patch>)],
    mut descendants: HashSet<String>,
) -> HashSet<String> {
    loop {
        let mut changed = false;
        for (metadata, _) in parsed {
            let message_id = clean_message_id(&metadata.message_id);
            if !descendants.contains(message_id) && metadata_descends_from(metadata, &descendants) {
                changed |= descendants.insert(message_id.to_string());
            }
        }
        if !changed {
            return descendants;
        }
    }
}

#[derive(Debug)]
struct ParsedSeriesHead {
    source_index: usize,
    message_id: String,
    author: String,
    body: String,
    total: u32,
    version: u32,
}

fn metadata_matches_series(metadata: &PatchsetMetadata, head: &ParsedSeriesHead) -> bool {
    metadata.total == head.total
        && metadata.version.unwrap_or(1) == head.version
        && authors_match(&head.author, &metadata.author)
}

fn has_matching_cover_ancestor(
    parsed: &[(PatchsetMetadata, Option<Patch>)],
    head: &PatchsetMetadata,
) -> bool {
    let cover_ids = parsed
        .iter()
        .filter(|(metadata, patch)| {
            metadata.is_patch_or_cover
                && metadata.index == 0
                && patch.is_none()
                && metadata.total == head.total
                && metadata.version.unwrap_or(1) == head.version.unwrap_or(1)
                && authors_match(&head.author, &metadata.author)
        })
        .map(|(metadata, _)| clean_message_id(&metadata.message_id).to_string())
        .collect::<HashSet<_>>();

    !cover_ids.is_empty()
        && expand_descendants(parsed, cover_ids).contains(clean_message_id(&head.message_id))
}

fn select_series_head(
    parsed: &[(PatchsetMetadata, Option<Patch>)],
    requested_message_id: &str,
) -> Result<ParsedSeriesHead> {
    let matching_heads = parsed
        .iter()
        .enumerate()
        .filter(|(_, (metadata, _))| clean_message_id(&metadata.message_id) == requested_message_id)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [head_index] = matching_heads.as_slice() else {
        if matching_heads.is_empty() {
            bail!("lore thread does not contain message ID {requested_message_id}");
        }
        bail!("lore thread contains duplicate message ID {requested_message_id}");
    };
    let head_index = *head_index;
    let (head, head_patch) = &parsed[head_index];
    if !head.is_patch_or_cover {
        bail!("Based-on message {requested_message_id} is not a patch series head");
    }
    match head.index {
        0 if head_patch.is_none() => {}
        1 if head_patch.is_some() && !has_matching_cover_ancestor(parsed, head) => {}
        _ => bail!("Based-on message {requested_message_id} is not the head of its series"),
    }
    if head.total == 0 {
        bail!("Based-on series {requested_message_id} has no patches");
    }
    if head.total as usize > MAX_PREREQUISITE_PATCH_IDS {
        bail!(
            "Based-on series {requested_message_id} contains more than {MAX_PREREQUISITE_PATCH_IDS} patches"
        );
    }

    Ok(ParsedSeriesHead {
        source_index: head_index,
        message_id: head.message_id.clone(),
        author: head.author.clone(),
        body: head.body.clone(),
        total: head.total,
        version: head.version.unwrap_or(1),
    })
}

fn select_series_patches(
    parsed: Vec<(PatchsetMetadata, Option<Patch>)>,
    requested_message_id: &str,
    head: &ParsedSeriesHead,
    descendants: &HashSet<String>,
) -> Result<Vec<(PatchsetMetadata, Patch)>> {
    let mut selected = Vec::with_capacity(head.total as usize);
    let mut seen_parts = HashSet::new();
    for (index, (metadata, patch)) in parsed.into_iter().enumerate() {
        if !metadata.is_patch_or_cover {
            continue;
        }
        let Some(patch) = patch else {
            continue;
        };
        if index != head.source_index
            && !descendants.contains(clean_message_id(&metadata.message_id))
        {
            continue;
        }
        if !metadata_matches_series(&metadata, head) {
            continue;
        }
        if metadata.index == 0 || metadata.index > head.total || metadata.index != patch.part_index
        {
            bail!(
                "Based-on series {requested_message_id} has invalid part {}/{}",
                metadata.index,
                head.total
            );
        }
        if !seen_parts.insert(metadata.index) {
            bail!(
                "Based-on series {requested_message_id} has duplicate part {}/{}",
                metadata.index,
                head.total
            );
        }
        selected.push((metadata, patch));
    }
    if selected.len() != head.total as usize {
        bail!(
            "Based-on series {requested_message_id} is incomplete: found {} of {} patches",
            selected.len(),
            head.total
        );
    }
    selected.sort_by_key(|(metadata, _)| metadata.index);
    Ok(selected)
}

async fn materialize_series_patches(
    selected: Vec<(PatchsetMetadata, Patch)>,
    requested_message_id: &str,
) -> Result<Vec<PrerequisitePatch>> {
    let patch_ids = calculate_git_patch_id_batch(
        selected
            .iter()
            .map(|(_, patch)| Some(patch.diff.as_str()))
            .collect(),
    )
    .await;
    let mut patches = Vec::with_capacity(selected.len());
    for ((metadata, patch), patch_id) in selected.into_iter().zip(patch_ids) {
        let git_patch_id = patch_id?.ok_or_else(|| {
            anyhow!(
                "Based-on series {requested_message_id} part {} has no stable patch ID",
                patch.part_index
            )
        })?;
        patches.push(PrerequisitePatch {
            git_patch_id,
            message_id: patch.message_id,
            subject: metadata.subject,
            author: metadata.author,
            date: metadata.date,
            diff: patch.diff,
        });
    }
    Ok(patches)
}

async fn series_from_mbox(raw: Vec<u8>, requested_message_id: &str) -> Result<PrerequisiteSeries> {
    let parsed = parse_mbox_messages(raw).await?;
    let requested_message_id = clean_message_id(requested_message_id);
    let head = select_series_head(&parsed, requested_message_id)?;
    let descendants = expand_descendants(
        &parsed,
        HashSet::from([clean_message_id(&head.message_id).to_string()]),
    );
    let selected = select_series_patches(parsed, requested_message_id, &head, &descendants)?;
    let patches = materialize_series_patches(selected, requested_message_id).await?;

    Ok(PrerequisiteSeries {
        local_patchset_id: None,
        message_id: head.message_id,
        body: head.body,
        patches,
    })
}

async fn patches_from_mbox(raw: Vec<u8>) -> Result<Vec<PrerequisitePatch>> {
    let parsed = parse_mbox_messages(raw).await?;

    let patch_ids = calculate_git_patch_id_batch(
        parsed
            .iter()
            .map(|(metadata, patch)| {
                if metadata.is_patch_or_cover {
                    patch.as_ref().map(|patch| patch.diff.as_str())
                } else {
                    None
                }
            })
            .collect(),
    )
    .await;
    let mut patches = Vec::new();
    let mut seen = HashSet::new();
    for ((metadata, patch), git_patch_id) in parsed.into_iter().zip(patch_ids) {
        if !metadata.is_patch_or_cover {
            continue;
        }
        let Some(patch) = patch else {
            continue;
        };
        let Some(git_patch_id) = git_patch_id? else {
            continue;
        };
        if !seen.insert(git_patch_id.clone()) {
            continue;
        }

        patches.push(PrerequisitePatch {
            git_patch_id,
            message_id: patch.message_id,
            subject: metadata.subject,
            author: metadata.author,
            date: metadata.date,
            diff: patch.diff,
        });
    }
    Ok(patches)
}

fn normalize_patch_id(patch_id: &str) -> Result<String> {
    if patch_id.len() != 40 || !patch_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid stable Git patch ID {patch_id}");
    }
    Ok(patch_id.to_ascii_lowercase())
}

async fn series_from_storage(stored: StoredPrerequisiteSeries) -> Result<PrerequisiteSeries> {
    if stored.patches.len() > MAX_PREREQUISITE_PATCH_IDS {
        bail!(
            "stored prerequisite series {} contains more than {MAX_PREREQUISITE_PATCH_IDS} patches",
            stored.message_id
        );
    }
    let calculations = calculate_git_patch_id_batch(
        stored
            .patches
            .iter()
            .map(|patch| patch.git_patch_id.is_none().then_some(patch.diff.as_str()))
            .collect(),
    )
    .await;
    let mut patches = Vec::with_capacity(stored.patches.len());
    for (patch, calculated) in stored.patches.into_iter().zip(calculations) {
        let git_patch_id = match patch.git_patch_id {
            Some(patch_id) => normalize_patch_id(&patch_id)?,
            None => calculated?.ok_or_else(|| {
                anyhow!(
                    "stored prerequisite {} part {} has no stable patch ID",
                    stored.message_id,
                    patch.part_index
                )
            })?,
        };
        patches.push(PrerequisitePatch {
            git_patch_id,
            message_id: patch.message_id,
            subject: patch.subject,
            author: patch.author,
            date: patch.date,
            diff: patch.diff,
        });
    }

    Ok(PrerequisiteSeries {
        local_patchset_id: Some(stored.patchset_id),
        message_id: stored.message_id,
        body: stored.body,
        patches,
    })
}

#[async_trait]
trait PrerequisiteRemote: Send + Sync {
    async fn fetch_series(&self, message_id: &str) -> Result<PrerequisiteSeries>;
    async fn search_patch_id(&self, patch_id: &str) -> Result<Vec<PrerequisitePatch>>;
}

struct LorePrerequisiteRemote;

#[async_trait]
impl PrerequisiteRemote for LorePrerequisiteRemote {
    async fn fetch_series(&self, message_id: &str) -> Result<PrerequisiteSeries> {
        info!("Fetching prerequisite series {} from lore", message_id);
        let raw = LoreMboxClient::new()?.fetch_thread(message_id).await?;
        series_from_mbox(raw, message_id).await
    }

    async fn search_patch_id(&self, patch_id: &str) -> Result<Vec<PrerequisitePatch>> {
        info!("Fetching prerequisite patch {} from lore", patch_id);
        let raw = LoreMboxClient::new()?.search_patch_id(patch_id).await?;
        patches_from_mbox(raw).await
    }
}

struct PrerequisiteResolver<'a, R: ?Sized> {
    db: &'a Database,
    remote: &'a R,
    remote_operations: usize,
    series_cache: HashMap<String, PrerequisiteSeries>,
    patch_cache: HashMap<String, PrerequisitePatch>,
    active_message_ids: HashSet<String>,
    active_patchset_ids: HashSet<i64>,
    seen_patch_ids: HashSet<String>,
    resolved: Vec<PrerequisitePatch>,
}

impl<'a, R: PrerequisiteRemote + ?Sized> PrerequisiteResolver<'a, R> {
    fn new(db: &'a Database, remote: &'a R) -> Self {
        Self {
            db,
            remote,
            remote_operations: 0,
            series_cache: HashMap::new(),
            patch_cache: HashMap::new(),
            active_message_ids: HashSet::new(),
            active_patchset_ids: HashSet::new(),
            seen_patch_ids: HashSet::new(),
            resolved: Vec::new(),
        }
    }

    fn count_remote_operation(&mut self) -> Result<()> {
        if self.remote_operations == MAX_LORE_OPERATIONS {
            bail!(
                "resolving prerequisites requires more than {MAX_LORE_OPERATIONS} remote Lore operations"
            );
        }
        self.remote_operations += 1;
        Ok(())
    }

    async fn load_series(&mut self, message_id: &str) -> Result<PrerequisiteSeries> {
        let message_id = clean_message_id(message_id);
        if let Some(series) = self.series_cache.get(message_id) {
            return Ok(series.clone());
        }

        let series = if let Some(stored) = self
            .db
            .get_stored_prerequisite_series(message_id, MAX_PREREQUISITE_PATCH_IDS)
            .await?
        {
            info!(
                "Resolved prerequisite series {} from local patchset {}",
                stored.message_id, stored.patchset_id
            );
            series_from_storage(stored).await?
        } else {
            self.count_remote_operation()?;
            self.remote
                .fetch_series(message_id)
                .await
                .with_context(|| {
                    format!("failed to fetch prerequisite series {message_id} from lore")
                })?
        };
        if clean_message_id(&series.message_id) != message_id {
            bail!(
                "lore returned series {} for requested message ID {message_id}",
                series.message_id
            );
        }

        self.series_cache
            .insert(message_id.to_string(), series.clone());
        Ok(series)
    }

    async fn load_patch(&mut self, patch_id: &str) -> Result<PrerequisitePatch> {
        let patch_id = normalize_patch_id(patch_id)?;
        if let Some(patch) = self.patch_cache.get(&patch_id) {
            return Ok(patch.clone());
        }
        if let Some((message_id, diff, subject, author, date)) =
            self.db.get_patch_by_git_patch_id(&patch_id).await?
        {
            info!(
                "Resolved prerequisite patch {} from local message {}",
                patch_id, message_id
            );
            return Ok(PrerequisitePatch {
                git_patch_id: patch_id,
                message_id,
                subject,
                author,
                date,
                diff,
            });
        }

        self.count_remote_operation()?;
        let fetched = self
            .remote
            .search_patch_id(&patch_id)
            .await
            .with_context(|| format!("failed to fetch prerequisite patch {patch_id} from lore"))?;
        for mut patch in fetched {
            patch.git_patch_id = normalize_patch_id(&patch.git_patch_id)?;
            self.patch_cache
                .entry(patch.git_patch_id.clone())
                .or_insert(patch);
        }
        self.patch_cache
            .get(&patch_id)
            .cloned()
            .ok_or_else(|| anyhow!("lore did not return prerequisite patch ID {patch_id}"))
    }

    fn append_patch(&mut self, mut patch: PrerequisitePatch) -> Result<()> {
        patch.git_patch_id = normalize_patch_id(&patch.git_patch_id)?;
        self.patch_cache
            .entry(patch.git_patch_id.clone())
            .or_insert_with(|| patch.clone());
        if !self.seen_patch_ids.insert(patch.git_patch_id.clone()) {
            return Ok(());
        }
        if self.resolved.len() == MAX_PREREQUISITE_PATCH_IDS {
            bail!(
                "resolved prerequisites contain more than {MAX_PREREQUISITE_PATCH_IDS} unique patches"
            );
        }
        self.resolved.push(patch);
        Ok(())
    }

    async fn resolve_patch_ids(&mut self, patch_ids: &[String]) -> Result<()> {
        for patch_id in patch_ids {
            let patch = self.load_patch(patch_id).await?;
            self.append_patch(patch)?;
        }
        Ok(())
    }

    async fn resolve_metadata(
        &mut self,
        metadata: PrerequisiteMetadata,
        depth: usize,
    ) -> Result<()> {
        match metadata {
            PrerequisiteMetadata::PatchIds(patch_ids) => self.resolve_patch_ids(&patch_ids).await,
            PrerequisiteMetadata::BasedOn(message_id) => {
                self.resolve_series(message_id, depth).await
            }
        }
    }

    fn resolve_series<'resolver>(
        &'resolver mut self,
        message_id: String,
        depth: usize,
    ) -> BoxFuture<'resolver, Result<()>> {
        Box::pin(async move {
            if depth > MAX_BASED_ON_DEPTH {
                bail!("Based-on dependency chain exceeds {MAX_BASED_ON_DEPTH} series");
            }
            let requested_id = clean_message_id(&message_id).to_string();
            if self.active_message_ids.contains(&requested_id) {
                bail!("Based-on dependency cycle includes {requested_id}");
            }

            let series = self.load_series(&requested_id).await?;
            let actual_id = clean_message_id(&series.message_id).to_string();
            if self.active_message_ids.contains(&actual_id)
                || series
                    .local_patchset_id
                    .is_some_and(|id| self.active_patchset_ids.contains(&id))
            {
                bail!("Based-on dependency cycle includes {actual_id}");
            }

            self.active_message_ids.insert(requested_id.clone());
            self.active_message_ids.insert(actual_id.clone());
            if let Some(patchset_id) = series.local_patchset_id {
                self.active_patchset_ids.insert(patchset_id);
            }

            let result = async {
                if let Some(metadata) = parse_prerequisite_metadata(&series.body)? {
                    self.resolve_metadata(metadata, depth + 1).await?;
                }
                for patch in series.patches {
                    self.append_patch(patch)?;
                }
                Ok(())
            }
            .await;

            self.active_message_ids.remove(&requested_id);
            self.active_message_ids.remove(&actual_id);
            if let Some(patchset_id) = series.local_patchset_id {
                self.active_patchset_ids.remove(&patchset_id);
            }
            result
        })
    }

    async fn resolve_target(
        mut self,
        target_patchset_id: i64,
        target_message_id: Option<&str>,
        metadata: PrerequisiteMetadata,
    ) -> Result<Vec<PrerequisitePatch>> {
        self.active_patchset_ids.insert(target_patchset_id);
        if let Some(message_id) = target_message_id {
            self.active_message_ids
                .insert(clean_message_id(message_id).to_string());
        }
        self.resolve_metadata(metadata, 1).await?;
        Ok(self.resolved)
    }
}

async fn resolve_prerequisites<R: PrerequisiteRemote + ?Sized>(
    db: &Database,
    remote: &R,
    target_patchset_id: i64,
    target_message_id: Option<&str>,
    body: &str,
) -> Result<Vec<PrerequisitePatch>> {
    let Some(metadata) = parse_prerequisite_metadata(body)? else {
        return Ok(Vec::new());
    };
    PrerequisiteResolver::new(db, remote)
        .resolve_target(target_patchset_id, target_message_id, metadata)
        .await
}

pub(crate) async fn resolve_prerequisites_from_lore(
    db: &Database,
    target_patchset_id: i64,
    target_message_id: Option<&str>,
    body: &str,
) -> Result<Vec<PrerequisitePatch>> {
    let remote = LorePrerequisiteRemote;
    resolve_prerequisites(db, &remote, target_patchset_id, target_message_id, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeRemote {
        series: HashMap<String, PrerequisiteSeries>,
        patch_results: HashMap<String, Vec<PrerequisitePatch>>,
        series_calls: AtomicUsize,
        patch_calls: AtomicUsize,
    }

    #[async_trait]
    impl PrerequisiteRemote for FakeRemote {
        async fn fetch_series(&self, message_id: &str) -> Result<PrerequisiteSeries> {
            self.series_calls.fetch_add(1, Ordering::SeqCst);
            self.series
                .get(message_id)
                .cloned()
                .ok_or_else(|| anyhow!("no fake series for {message_id}"))
        }

        async fn search_patch_id(&self, patch_id: &str) -> Result<Vec<PrerequisitePatch>> {
            self.patch_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .patch_results
                .get(patch_id)
                .cloned()
                .unwrap_or_default())
        }
    }

    fn sample_patch(id: &str, message_id: &str) -> PrerequisitePatch {
        PrerequisitePatch {
            git_patch_id: id.to_string(),
            message_id: message_id.to_string(),
            subject: "[PATCH] prerequisite".to_string(),
            author: "Author <author@example.com>".to_string(),
            date: 1_700_000_000,
            diff: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n".to_string(),
        }
    }

    fn sample_series(
        message_id: &str,
        body: &str,
        patches: Vec<PrerequisitePatch>,
    ) -> PrerequisiteSeries {
        PrerequisiteSeries {
            local_patchset_id: None,
            message_id: message_id.to_string(),
            body: body.to_string(),
            patches,
        }
    }

    fn patch_body(file: &str, old: &str, new: &str) -> String {
        format!(
            "Patch body\n\ndiff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-{old}\n+{new}\n"
        )
    }

    fn mbox_message(message_id: &str, subject: &str, parent: Option<&str>, body: &str) -> String {
        let reply_headers = parent
            .map(|parent| format!("In-Reply-To: <{parent}>\nReferences: <{parent}>\n"))
            .unwrap_or_default();
        format!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\nFrom: Author <author@example.com>\nDate: Tue, 14 Nov 2023 22:13:20 +0000\nMessage-ID: <{message_id}>\nSubject: {subject}\n{reply_headers}Content-Type: text/plain; charset=utf-8\n\n{body}\n"
        )
    }

    async fn memory_db() -> Result<Database> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await?;
        db.migrate().await?;
        Ok(db)
    }

    async fn insert_local_patch(
        db: &Database,
        message_id: &str,
        diff: &str,
        patch_id: &str,
    ) -> Result<i64> {
        let thread_id = db.create_thread("root", "subject", 1).await?;
        db.create_message(
            message_id,
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH] local",
            1,
            "body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let patchset_id = db
            .create_patchset(
                thread_id, None, "root", "subject", "author", 1, 1, 1, "", "", None, 1, None,
                false, None, None,
            )
            .await?
            .ok_or_else(|| anyhow!("test patchset was not created"))?;
        db.create_patch_with_git_patch_id(patchset_id, message_id, 1, diff, Some(patch_id))
            .await?;
        Ok(patchset_id)
    }

    async fn insert_local_single_series(
        db: &Database,
        message_id: &str,
        body: &str,
        patch_id: &str,
    ) -> Result<i64> {
        let thread_id = db.create_thread(message_id, "local base", 1).await?;
        db.create_message(
            message_id,
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH] local base",
            1,
            body,
            "",
            "",
            None,
            None,
        )
        .await?;
        let patchset_id = db
            .create_patchset(
                thread_id,
                Some(message_id),
                message_id,
                "[PATCH] local base",
                "Author <author@example.com>",
                1,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await?
            .ok_or_else(|| anyhow!("test prerequisite series was not created"))?;
        db.create_patch_with_git_patch_id(
            patchset_id,
            message_id,
            1,
            "diff --git a/local b/local\n--- a/local\n+++ b/local\n@@ -1 +1 @@\n-old\n+new\n",
            Some(patch_id),
        )
        .await?;
        Ok(patchset_id)
    }

    async fn insert_incomplete_local_series(db: &Database, message_id: &str) -> Result<i64> {
        let thread_id = db.create_thread(message_id, "incomplete base", 1).await?;
        db.create_message(
            message_id,
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH 0/2] incomplete base",
            1,
            "cover body",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "incomplete-part@example.com",
            thread_id,
            Some(message_id),
            "Author <author@example.com>",
            "[PATCH 1/2] incomplete part",
            2,
            "patch body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let patchset_id = db
            .create_patchset(
                thread_id,
                Some(message_id),
                "incomplete-part@example.com",
                "[PATCH 1/2] incomplete part",
                "Author <author@example.com>",
                2,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await?
            .ok_or_else(|| anyhow!("incomplete test series was not created"))?;
        db.create_patch(
            patchset_id,
            "incomplete-part@example.com",
            1,
            "incomplete diff",
        )
        .await?;
        Ok(patchset_id)
    }

    #[tokio::test]
    async fn migration_adds_patch_id_column_and_index_to_existing_database() -> Result<()> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await?;
        db.conn
            .execute_batch(
                "CREATE TABLE patches (
                    id INTEGER PRIMARY KEY,
                    patchset_id INTEGER NOT NULL,
                    message_id TEXT NOT NULL UNIQUE,
                    part_index INTEGER,
                    diff TEXT
                );
                PRAGMA user_version = 11;",
            )
            .await?;

        db.migrate().await?;

        let mut columns = db.conn.query("PRAGMA table_info(patches)", ()).await?;
        let mut found_column = false;
        while let Some(row) = columns.next().await? {
            let name: String = row.get(1)?;
            found_column |= name == "git_patch_id";
        }
        assert!(found_column);

        let mut indexes = db
            .conn
            .query(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_patches_git_patch_id'",
                (),
            )
            .await?;
        assert!(indexes.next().await?.is_some());

        let mut version = db.conn.query("PRAGMA user_version", ()).await?;
        let version: u32 = version.next().await?.expect("user version row").get(0)?;
        assert_eq!(version, 13);
        Ok(())
    }

    #[test]
    fn parses_ordered_unique_patch_ids() -> Result<()> {
        let first = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let body = format!(
            "prerequisite-patch-id: {first}\r\nprerequisite-patch-id: {second}\r\nprerequisite-patch-id: {first}\r\n> prerequisite-patch-id: cccccccccccccccccccccccccccccccccccccccc\r\n prerequisite-patch-id: dddddddddddddddddddddddddddddddddddddddd\r\nprerequisite-patch-id: short\r\n"
        );

        assert_eq!(
            parse_prerequisite_patch_ids(&body)?,
            vec![first.to_ascii_lowercase(), second.to_string()]
        );
        Ok(())
    }

    #[test]
    fn parses_based_on_without_valid_b4_patch_ids() -> Result<()> {
        for b4_metadata in [
            "",
            "prerequisite-patch-id: short",
            "> prerequisite-patch-id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            " prerequisite-patch-id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "+prerequisite-patch-id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "prerequisite-message-id: <other@example.com>\nprerequisite-change-id: other:v2",
        ] {
            let body = format!(
                "bAsEd-On: <base@example.com>\nBased-on: base@example.com\n{b4_metadata}\n"
            );
            assert_eq!(
                parse_prerequisite_metadata(&body)?,
                Some(PrerequisiteMetadata::BasedOn(
                    "base@example.com".to_string()
                ))
            );
        }
        Ok(())
    }

    #[test]
    fn prefers_b4_patch_ids_over_based_on_metadata() -> Result<()> {
        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let b4_metadata = format!(
            "prerequisite-patch-id: {second}\nprerequisite-patch-id: {first}\nprerequisite-patch-id: {second}"
        );
        for based_on in [
            "Based-on: <base@example.com>",
            "Based-on:",
            "Based-on: <unclosed@example.com",
            "Based-on: one@example.com\nBased-on: two@example.com",
        ] {
            for body in [
                format!("{based_on}\n{b4_metadata}"),
                format!("{b4_metadata}\n{based_on}"),
            ] {
                assert_eq!(
                    parse_prerequisite_metadata(&body)?,
                    Some(PrerequisiteMetadata::PatchIds(vec![
                        second.to_string(),
                        first.to_string()
                    ]))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn ignores_quoted_and_indented_based_on_lines() -> Result<()> {
        let body = "> Based-on: quoted@example.com\n Based-on: indented@example.com\n\
                    +Based-on: diff@example.com\n";

        assert_eq!(parse_prerequisite_metadata(body)?, None);
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_or_malformed_based_on_values() {
        let too_long = format!("{}@example.com", "a".repeat(999));
        let too_long_line = format!("Based-on: {too_long}");
        let invalid = [
            "Based-on:",
            "Based-on: <base@example.com",
            "Based-on: base@example.com>",
            "Based-on: base @example.com",
            "Based-on: base@example.com comment",
            "Based-on: bése@example.com",
            "Based-on: not-a-message-id",
            "Based-on: <base@example.com> trailing",
            too_long_line.as_str(),
            "Based-on: one@example.com\nBased-on: two@example.com",
        ];

        for body in invalid {
            assert!(
                parse_prerequisite_metadata(body).is_err(),
                "metadata should be rejected: {body:?}"
            );
        }
    }

    #[test]
    fn rejects_too_many_prerequisite_patch_ids() {
        let body = (0..=MAX_PREREQUISITE_PATCH_IDS)
            .map(|index| format!("prerequisite-patch-id: {index:040x}"))
            .collect::<Vec<_>>()
            .join("\n");

        let error = parse_prerequisite_patch_ids(&body)
            .expect_err("metadata above the prerequisite limit should fail");
        assert!(error.to_string().contains("more than 128"));
    }

    #[tokio::test]
    async fn rejects_resolution_above_prerequisite_limit() -> Result<()> {
        let db = memory_db().await?;
        let mut body = (0..=MAX_PREREQUISITE_PATCH_IDS)
            .map(|index| format!("prerequisite-patch-id: {index:040x}"))
            .collect::<Vec<_>>();
        body.push("Based-on: base@example.com".to_string());
        let remote = FakeRemote::default();

        let error = resolve_prerequisites(&db, &remote, 1, None, &body.join("\n"))
            .await
            .expect_err("resolution above the prerequisite limit should fail");

        assert!(error.to_string().contains("more than 128"));
        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn calculates_stable_git_patch_id() -> Result<()> {
        let diff = "diff --git a/file b/file\n\
                    --- a/file\n\
                    +++ b/file\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n";
        let id = calculate_git_patch_id(diff).await?;
        assert!(id.is_some());
        assert_eq!(id.unwrap().len(), 40);
        assert_eq!(calculate_git_patch_id("").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn batch_patch_ids_preserve_empty_slots() -> Result<()> {
        let diff = "diff --git a/file b/file\n\
                    --- a/file\n\
                    +++ b/file\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n";

        let ids = calculate_git_patch_id_batch(vec![None, Some(diff), None])
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], None);
        assert!(ids[1].is_some());
        assert_eq!(ids[2], None);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_patch_id_output_while_writing_input() -> Result<()> {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "head -c 131072 /dev/zero; cat >/dev/null"]);
        let input = vec![b'x'; 131_072];

        let output = run_git_patch_id_command(command, &input, Duration::from_secs(5)).await?;

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131_072);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_child_failure_after_broken_stdin_pipe() -> Result<()> {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "exec 0<&-; printf 'specific patch-id failure' >&2; exit 42",
        ]);
        let input = vec![b'x'; 1024 * 1024];

        let output = run_git_patch_id_command(command, &input, Duration::from_secs(5)).await?;

        assert_eq!(output.status.code(), Some(42));
        assert_eq!(output.stderr, b"specific patch-id failure");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_patch_id_command() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; exec sleep 60"]);

        let error = run_git_patch_id_command(command, b"patch", Duration::from_millis(50))
            .await
            .expect_err("long-running patch-id command should time out");

        assert!(error.to_string().contains("timed out after 50ms"));
    }

    #[tokio::test]
    async fn parses_lore_mbox() -> Result<()> {
        let body = "Patch body\n\n---\n file | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n\ndiff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
        let patch_id = calculate_git_patch_id(body)
            .await?
            .expect("test patch should have a stable ID");
        let raw_mbox = format!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\nFrom: Author <author@example.com>\nDate: Tue, 14 Nov 2023 22:13:20 +0000\nMessage-ID: <patch@example.com>\nSubject: [PATCH] prerequisite\nContent-Type: text/plain; charset=utf-8\n\n{body}"
        );
        let patches = patches_from_mbox(raw_mbox.into_bytes()).await?;

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].git_patch_id, patch_id);
        assert_eq!(patches[0].message_id, "patch@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn extracts_only_the_named_complete_lore_series() -> Result<()> {
        let raw = [
            mbox_message(
                "cover@example.com",
                "[PATCH v2 0/2] base series",
                None,
                "Based-on: older@example.com",
            ),
            mbox_message(
                "part2@example.com",
                "[PATCH v2 2/2] second",
                Some("part1@example.com"),
                &patch_body("second", "old", "new"),
            ),
            mbox_message(
                "unrelated@example.com",
                "[PATCH] unrelated",
                Some("cover@example.com"),
                &patch_body("unrelated", "old", "new"),
            ),
            mbox_message(
                "part1@example.com",
                "[PATCH v2 1/2] first",
                Some("cover@example.com"),
                &patch_body("first", "old", "new"),
            ),
            mbox_message(
                "review@example.com",
                "Re: [PATCH v2 1/2] first",
                Some("part1@example.com"),
                &patch_body("quoted", "old", "new"),
            ),
        ]
        .concat();

        let series = series_from_mbox(raw.into_bytes(), "cover@example.com").await?;

        assert_eq!(series.message_id, "cover@example.com");
        assert!(series.body.contains("Based-on: older@example.com"));
        assert_eq!(
            series
                .patches
                .iter()
                .map(|patch| patch.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["part1@example.com", "part2@example.com"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn extracts_coverless_single_patch_series() -> Result<()> {
        let raw = mbox_message(
            "single@example.com",
            "[PATCH] one patch",
            None,
            &patch_body("single", "old", "new"),
        );

        let series = series_from_mbox(raw.into_bytes(), "single@example.com").await?;

        assert_eq!(series.patches.len(), 1);
        assert_eq!(series.patches[0].message_id, "single@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn extracts_coverless_multi_patch_series_from_first_patch() -> Result<()> {
        let first = mbox_message(
            "first@example.com",
            "[PATCH 1/2] first",
            None,
            &patch_body("first", "old", "new"),
        );
        let second = mbox_message(
            "second@example.com",
            "[PATCH 2/2] second",
            Some("first@example.com"),
            &patch_body("second", "old", "new"),
        );

        let series =
            series_from_mbox(format!("{first}{second}").into_bytes(), "first@example.com").await?;

        assert_eq!(series.patches.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_first_patch_when_matching_cover_is_ancestor() {
        let cover = mbox_message(
            "cover@example.com",
            "[PATCH 0/2] covered series",
            None,
            "cover body",
        );
        let first = mbox_message(
            "first@example.com",
            "[PATCH 1/2] first",
            Some("cover@example.com"),
            &patch_body("first", "old", "new"),
        );
        let second = mbox_message(
            "second@example.com",
            "[PATCH 2/2] second",
            Some("first@example.com"),
            &patch_body("second", "old", "new"),
        );

        let error = series_from_mbox(
            format!("{cover}{first}{second}").into_bytes(),
            "first@example.com",
        )
        .await
        .expect_err("part one of a covered series is not its head");

        assert!(error.to_string().contains("not the head"));
    }

    #[tokio::test]
    async fn rejects_incomplete_duplicate_and_non_head_lore_series() {
        let cover = mbox_message(
            "cover@example.com",
            "[PATCH 0/2] base series",
            None,
            "cover body",
        );
        let first = mbox_message(
            "first@example.com",
            "[PATCH 1/2] first",
            Some("cover@example.com"),
            &patch_body("first", "old", "new"),
        );
        let second = mbox_message(
            "second@example.com",
            "[PATCH 2/2] second",
            Some("cover@example.com"),
            &patch_body("second", "old", "new"),
        );

        let incomplete =
            series_from_mbox(format!("{cover}{first}").into_bytes(), "cover@example.com")
                .await
                .expect_err("missing series parts must fail");
        assert!(incomplete.to_string().contains("incomplete"));

        let duplicate_first = mbox_message(
            "duplicate@example.com",
            "[PATCH 1/2] duplicate first",
            Some("cover@example.com"),
            &patch_body("duplicate", "old", "new"),
        );
        let duplicate = series_from_mbox(
            format!("{cover}{first}{duplicate_first}{second}").into_bytes(),
            "cover@example.com",
        )
        .await
        .expect_err("duplicate series parts must fail");
        assert!(duplicate.to_string().contains("duplicate part"));

        let non_head = series_from_mbox(
            format!("{cover}{first}{second}").into_bytes(),
            "second@example.com",
        )
        .await
        .expect_err("a later patch must not name a Based-on series");
        assert!(non_head.to_string().contains("not the head"));
    }

    #[tokio::test]
    async fn resolves_local_patch_without_fetching_lore() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        insert_local_patch(&db, "local@example.com", "local diff", patch_id).await?;
        let remote = FakeRemote::default();

        let resolved = resolve_prerequisites(
            &db,
            &remote,
            99,
            None,
            &format!("prerequisite-patch-id: {patch_id}"),
        )
        .await?;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].message_id, "local@example.com");
        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_complete_based_on_series_locally() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        insert_local_single_series(&db, "base@example.com", "base body", patch_id).await?;
        let remote = FakeRemote::default();

        let resolved = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: base@example.com",
        )
        .await?;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].message_id, "base@example.com");
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn calculates_missing_patch_ids_for_local_based_on_series() -> Result<()> {
        let db = memory_db().await?;
        let patchset_id = insert_local_single_series(
            &db,
            "base@example.com",
            "base body",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await?;
        db.conn
            .execute(
                "UPDATE patches SET git_patch_id = NULL WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let remote = FakeRemote::default();

        let resolved = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: base@example.com",
        )
        .await?;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].git_patch_id.len(), 40);
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn falls_back_to_lore_for_incomplete_local_series() -> Result<()> {
        let db = memory_db().await?;
        insert_incomplete_local_series(&db, "base@example.com").await?;
        let remote_patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let remote = FakeRemote {
            series: HashMap::from([(
                "base@example.com".to_string(),
                sample_series(
                    "base@example.com",
                    "",
                    vec![sample_patch(remote_patch_id, "remote-base@example.com")],
                ),
            )]),
            ..FakeRemote::default()
        };

        let resolved = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: base@example.com",
        )
        .await?;

        assert_eq!(resolved[0].git_patch_id, remote_patch_id);
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_only_b4_prerequisites_in_declared_order() -> Result<()> {
        let db = memory_db().await?;
        let first = sample_patch(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "first@example.com",
        );
        let second = sample_patch(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "second@example.com",
        );
        let extra = sample_patch(
            "cccccccccccccccccccccccccccccccccccccccc",
            "extra@example.com",
        );
        let remote = FakeRemote {
            series: HashMap::from([(
                "base@example.com".to_string(),
                sample_series(
                    "base@example.com",
                    "",
                    vec![first.clone(), second.clone(), extra.clone()],
                ),
            )]),
            patch_results: HashMap::from([(
                second.git_patch_id.clone(),
                vec![first.clone(), extra, second.clone()],
            )]),
            ..FakeRemote::default()
        };
        let body = format!(
            "Based-on: base@example.com\nprerequisite-patch-id: {}\nprerequisite-patch-id: {}\nprerequisite-patch-id: {}",
            second.git_patch_id, first.git_patch_id, second.git_patch_id
        );

        let resolved = resolve_prerequisites(&db, &remote, 99, None, &body).await?;

        assert_eq!(resolved, vec![second, first]);
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 0);
        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn does_not_fall_back_to_based_on_when_b4_resolution_fails() -> Result<()> {
        let db = memory_db().await?;
        let wanted = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let remote = FakeRemote {
            series: HashMap::from([(
                "base@example.com".to_string(),
                sample_series(
                    "base@example.com",
                    "",
                    vec![sample_patch(wanted, "base-patch@example.com")],
                ),
            )]),
            ..FakeRemote::default()
        };
        let body = format!("Based-on: base@example.com\nprerequisite-patch-id: {wanted}");

        let error = resolve_prerequisites(&db, &remote, 99, None, &body)
            .await
            .expect_err("a missing b4 prerequisite must not select Based-on instead");

        assert!(error.to_string().contains(wanted));
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 0);
        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_nested_dependencies_with_b4_precedence_and_deduplication() -> Result<()> {
        let db = memory_db().await?;
        let older_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let middle_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let older_b4_id = "cccccccccccccccccccccccccccccccccccccccc";
        let remote = FakeRemote {
            series: HashMap::from([
                (
                    "older@example.com".to_string(),
                    sample_series(
                        "older@example.com",
                        &format!(
                            "Based-on: target@example.com\nprerequisite-patch-id: {older_b4_id}"
                        ),
                        vec![sample_patch(older_id, "older-patch@example.com")],
                    ),
                ),
                (
                    "middle@example.com".to_string(),
                    sample_series(
                        "middle@example.com",
                        "Based-on: older@example.com",
                        vec![
                            sample_patch(older_id, "duplicate-older-patch@example.com"),
                            sample_patch(middle_id, "middle-patch@example.com"),
                        ],
                    ),
                ),
            ]),
            patch_results: HashMap::from([(
                older_b4_id.to_string(),
                vec![sample_patch(older_b4_id, "older-b4@example.com")],
            )]),
            ..FakeRemote::default()
        };
        let resolved = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: middle@example.com",
        )
        .await?;

        assert_eq!(
            resolved
                .iter()
                .map(|patch| patch.git_patch_id.as_str())
                .collect::<Vec<_>>(),
            vec![older_b4_id, older_id, middle_id]
        );
        assert_eq!(resolved[1].message_id, "older-patch@example.com");
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 2);
        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_nested_and_target_based_on_cycles() -> Result<()> {
        let db = memory_db().await?;
        let remote = FakeRemote {
            series: HashMap::from([
                (
                    "one@example.com".to_string(),
                    sample_series(
                        "one@example.com",
                        "Based-on: two@example.com",
                        vec![sample_patch(
                            "1111111111111111111111111111111111111111",
                            "one-patch@example.com",
                        )],
                    ),
                ),
                (
                    "two@example.com".to_string(),
                    sample_series(
                        "two@example.com",
                        "Based-on: one@example.com",
                        vec![sample_patch(
                            "2222222222222222222222222222222222222222",
                            "two-patch@example.com",
                        )],
                    ),
                ),
            ]),
            ..FakeRemote::default()
        };

        let nested = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: one@example.com",
        )
        .await
        .expect_err("a nested dependency cycle must fail");
        assert!(nested.to_string().contains("cycle"));

        let target_remote = FakeRemote::default();
        let target = resolve_prerequisites(
            &db,
            &target_remote,
            99,
            Some("target@example.com"),
            "Based-on: target@example.com",
        )
        .await
        .expect_err("a dependency on the target itself must fail");
        assert!(target.to_string().contains("cycle"));
        assert_eq!(target_remote.series_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn bounds_based_on_depth() -> Result<()> {
        let db = memory_db().await?;
        let series = (1..=MAX_BASED_ON_DEPTH + 1)
            .map(|depth| {
                let message_id = format!("series-{depth}@example.com");
                let body = if depth <= MAX_BASED_ON_DEPTH {
                    format!("Based-on: series-{}@example.com", depth + 1)
                } else {
                    String::new()
                };
                (
                    message_id.clone(),
                    sample_series(
                        &message_id,
                        &body,
                        vec![sample_patch(
                            &format!("{depth:040x}"),
                            &format!("patch-{depth}@example.com"),
                        )],
                    ),
                )
            })
            .collect();
        let remote = FakeRemote {
            series,
            ..FakeRemote::default()
        };

        let error = resolve_prerequisites(
            &db,
            &remote,
            99,
            Some("target@example.com"),
            "Based-on: series-1@example.com",
        )
        .await
        .expect_err("a chain deeper than the configured limit must fail");

        assert!(error.to_string().contains("exceeds 8 series"));
        assert_eq!(
            remote.series_calls.load(Ordering::SeqCst),
            MAX_BASED_ON_DEPTH
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_preserves_existing_patch_id() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "local@example.com";
        let diff = "local diff";
        let patchset_id = insert_local_patch(&db, message_id, diff, patch_id).await?;

        db.create_patch(patchset_id, message_id, 1, diff).await?;

        let stored = db.get_patch_by_git_patch_id(patch_id).await?;
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().0, message_id);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_compares_decompressed_diff() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "compressed@example.com";
        let diff = "x".repeat(2048);
        let patchset_id = insert_local_patch(&db, message_id, &diff, patch_id).await?;

        // Simulate an uncompressed row that the background compressor has not
        // processed yet, then re-ingest it through the compressed write path.
        db.conn
            .execute(
                "UPDATE patches SET diff = ? WHERE message_id = ?",
                libsql::params![diff.clone(), message_id],
            )
            .await?;
        db.create_patch(patchset_id, message_id, 1, &diff).await?;

        let stored = db
            .get_patch_by_git_patch_id(patch_id)
            .await?
            .expect("stable patch ID should be preserved");
        assert_eq!(stored.1, diff);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_rejects_invalid_stored_patch_id_type() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "invalid-id@example.com";
        let diff = "local diff";
        let patchset_id = insert_local_patch(&db, message_id, diff, patch_id).await?;
        db.conn
            .execute(
                "UPDATE patches SET git_patch_id = ? WHERE message_id = ?",
                libsql::params![libsql::Value::Blob(vec![0xff]), message_id],
            )
            .await?;

        assert!(
            db.create_patch(patchset_id, message_id, 1, diff)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn patch_lookup_rejects_invalid_message_metadata_type() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "invalid-subject@example.com";
        insert_local_patch(&db, message_id, "local diff", patch_id).await?;
        db.conn
            .execute(
                "UPDATE messages SET subject = ? WHERE message_id = ?",
                libsql::params![libsql::Value::Blob(vec![0xff]), message_id],
            )
            .await?;

        assert!(db.get_patch_by_git_patch_id(patch_id).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn patch_reingestion_rolls_back_all_updates_on_error() -> Result<()> {
        let db = memory_db().await?;
        let old_patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new_patch_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let message_id = "rollback@example.com";
        let old_diff = "old diff";
        let patchset_id = insert_local_patch(&db, message_id, old_diff, old_patch_id).await?;
        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Incomplete' WHERE id = ?",
                [patchset_id],
            )
            .await?;
        db.conn
            .execute_batch(
                "CREATE TRIGGER reject_patchset_update
                 BEFORE UPDATE ON patchsets BEGIN
                     SELECT RAISE(ABORT, 'test patchset update failure');
                 END;",
            )
            .await?;

        assert!(
            db.create_patch_with_git_patch_id(
                patchset_id,
                message_id,
                1,
                "new diff",
                Some(new_patch_id),
            )
            .await
            .is_err()
        );
        db.conn
            .execute("DROP TRIGGER reject_patchset_update", ())
            .await?;

        let stored = db
            .get_patch_by_git_patch_id(old_patch_id)
            .await?
            .expect("failed re-ingestion should preserve the old patch");
        assert_eq!(stored.1, old_diff);
        assert!(db.get_patch_by_git_patch_id(new_patch_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn changed_duplicate_without_patch_id_clears_stale_id() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "local@example.com";
        let patchset_id = insert_local_patch(&db, message_id, "old diff", patch_id).await?;

        db.create_patch(patchset_id, message_id, 1, "changed diff")
            .await?;

        assert!(db.get_patch_by_git_patch_id(patch_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn lore_result_is_cached_for_later_patch_ids() -> Result<()> {
        let db = memory_db().await?;
        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let remote = FakeRemote {
            patch_results: HashMap::from([(
                first.to_string(),
                vec![
                    sample_patch(first, "first@example.com"),
                    sample_patch(second, "second@example.com"),
                ],
            )]),
            ..FakeRemote::default()
        };
        let body = format!("prerequisite-patch-id: {first}\nprerequisite-patch-id: {second}");

        let resolved = resolve_prerequisites(&db, &remote, 99, None, &body).await?;

        assert_eq!(remote.patch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved[0].git_patch_id, first);
        assert_eq!(resolved[1].git_patch_id, second);
        Ok(())
    }

    #[tokio::test]
    async fn bounds_lore_searches_per_resolution() -> Result<()> {
        let db = memory_db().await?;
        let patch_ids = (0..=MAX_LORE_OPERATIONS)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let patch_results = patch_ids
            .iter()
            .map(|patch_id| {
                (
                    patch_id.clone(),
                    vec![sample_patch(patch_id, &format!("{patch_id}@example.com"))],
                )
            })
            .collect();
        let remote = FakeRemote {
            patch_results,
            ..FakeRemote::default()
        };
        let body = patch_ids
            .iter()
            .map(|patch_id| format!("prerequisite-patch-id: {patch_id}"))
            .collect::<Vec<_>>()
            .join("\n");

        let error = resolve_prerequisites(&db, &remote, 99, None, &body)
            .await
            .expect_err("resolution above the lore search limit should fail");

        assert!(
            error
                .to_string()
                .contains("more than 8 remote Lore operations")
        );
        assert_eq!(
            remote.patch_calls.load(Ordering::SeqCst),
            MAX_LORE_OPERATIONS
        );
        Ok(())
    }

    #[tokio::test]
    async fn shares_lore_limit_between_series_and_patch_searches() -> Result<()> {
        let db = memory_db().await?;
        let patch_ids = (0..MAX_LORE_OPERATIONS)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let base_body = patch_ids
            .iter()
            .map(|patch_id| format!("prerequisite-patch-id: {patch_id}"))
            .collect::<Vec<_>>()
            .join("\n");
        let remote = FakeRemote {
            series: HashMap::from([(
                "base@example.com".to_string(),
                sample_series(
                    "base@example.com",
                    &base_body,
                    vec![sample_patch(
                        "ffffffffffffffffffffffffffffffffffffffff",
                        "base-patch@example.com",
                    )],
                ),
            )]),
            patch_results: patch_ids
                .iter()
                .map(|patch_id| {
                    (
                        patch_id.clone(),
                        vec![sample_patch(patch_id, &format!("{patch_id}@example.com"))],
                    )
                })
                .collect(),
            ..FakeRemote::default()
        };
        let error = resolve_prerequisites(&db, &remote, 99, None, "Based-on: base@example.com")
            .await
            .expect_err("combined Lore operations above the limit must fail");

        assert!(
            error
                .to_string()
                .contains("more than 8 remote Lore operations")
        );
        assert_eq!(remote.series_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            remote.patch_calls.load(Ordering::SeqCst),
            MAX_LORE_OPERATIONS - 1
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_lore_result_without_exact_patch_id() -> Result<()> {
        let db = memory_db().await?;
        let wanted = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let remote = FakeRemote::default();
        let error = resolve_prerequisites(
            &db,
            &remote,
            99,
            None,
            &format!("prerequisite-patch-id: {wanted}"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(wanted));
        Ok(())
    }
}
