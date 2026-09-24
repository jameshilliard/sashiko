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

use crate::db::Database;
use crate::settings::ForgeSettings;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

const GITHUB_API_BASE: &str = "https://api.github.com";

pub struct ForgeWorker {
    db: Arc<Database>,
    settings: ForgeSettings,
    max_retries: u32,
    cached_token: Mutex<Option<(String, i64)>>,
}

impl ForgeWorker {
    pub fn new(db: Arc<Database>, settings: ForgeSettings, max_retries: u32) -> Self {
        Self {
            db,
            settings,
            max_retries,
            cached_token: Mutex::new(None),
        }
    }

    fn load_private_key(&self) -> Result<String, String> {
        if let Some(ref pem) = self.settings.app_private_key
            && !pem.trim().is_empty()
        {
            return Ok(pem.clone());
        }
        if let Some(ref path) = self.settings.app_private_key_path
            && !path.trim().is_empty()
        {
            return std::fs::read_to_string(path).map_err(|e| {
                format!("failed to read GitHub App private key from {}: {}", path, e)
            });
        }
        Err("no GitHub App private key configured".to_string())
    }

    async fn invalidate_cached_token(&self) {
        let mut guard = self.cached_token.lock().await;
        *guard = None;
    }

    async fn resolve_token(&self, client: &reqwest::Client) -> Result<String, String> {
        if let (Some(app_id), Some(installation_id)) =
            (self.settings.app_id, self.settings.installation_id)
        {
            let now = chrono::Utc::now().timestamp();
            {
                let guard = self.cached_token.lock().await;
                if let Some((ref token, expires_at)) = *guard
                    && now + 300 < expires_at
                {
                    return Ok(token.clone());
                }
            }

            let pem = self.load_private_key()?;
            let jwt = crate::forge::mint_github_app_jwt(app_id, &pem)?;
            let token = crate::forge::exchange_github_installation_token(
                client,
                GITHUB_API_BASE,
                installation_id,
                &jwt,
            )
            .await?;

            let mut guard = self.cached_token.lock().await;
            *guard = Some((token.clone(), now + 3600));
            return Ok(token);
        }

        if let Some(ref token) = self.settings.api_token
            && !token.trim().is_empty()
        {
            return Ok(token.clone());
        }

        Err("neither GitHub App credentials nor api_token configured".to_string())
    }

    fn backoff_seconds(retry_count: i64) -> i64 {
        match retry_count {
            0 => 5,
            1 => 30,
            _ => 180,
        }
    }

    fn is_permanent_failure(err: &str) -> bool {
        err.contains("404 Not Found") || err.contains("422 Unprocessable")
    }

    pub async fn run(&self) {
        info!("Starting Forge Worker...");
        let client = reqwest::Client::new();
        loop {
            if let Err(e) = self.db.sweep_ghost_forge_outbox().await {
                error!("Failed to sweep ghost forge outbox entries: {}", e);
            }

            match self.db.lock_pending_forge_outbox().await {
                Ok(Some(entry)) => {
                    if entry.provider != "github" {
                        let msg = format!("unsupported forge provider: {}", entry.provider);
                        error!("Forge outbox ID {}: {}", entry.id, msg);
                        if let Err(db_err) = self.db.mark_forge_outbox_failed(entry.id, &msg).await
                        {
                            error!(
                                "Failed to mark forge outbox {} as failed: {}",
                                entry.id, db_err
                            );
                        }
                        continue;
                    }

                    info!(
                        "Processing forge outbox ID {} for {}#{}",
                        entry.id, entry.repo, entry.pr_number
                    );

                    let token = match self.resolve_token(&client).await {
                        Ok(t) => t,
                        Err(e) => {
                            error!(
                                "Failed to resolve GitHub token for outbox ID {}: {}",
                                entry.id, e
                            );
                            self.handle_failure(entry.id, entry.retry_count, &e).await;
                            continue;
                        }
                    };

                    match crate::forge::post_github_pr_comment(
                        &client,
                        GITHUB_API_BASE,
                        &entry.repo,
                        entry.pr_number,
                        &token,
                        &entry.body,
                    )
                    .await
                    {
                        Ok(status) => {
                            info!(
                                "Posted GitHub PR comment for {}#{} (outbox ID {}, HTTP {})",
                                entry.repo, entry.pr_number, entry.id, status
                            );
                            if let Err(e) = self.db.mark_forge_outbox_sent(entry.id).await {
                                error!("Failed to mark forge outbox {} as sent: {}", entry.id, e);
                            }
                        }
                        Err(e) => {
                            if e.contains("401 Unauthorized") {
                                warn!(
                                    "GitHub returned 401; invalidating cached installation token"
                                );
                                self.invalidate_cached_token().await;
                            }
                            error!(
                                "Failed to post GitHub PR comment for outbox ID {}: {}",
                                entry.id, e
                            );
                            self.handle_failure(entry.id, entry.retry_count, &e).await;
                        }
                    }
                }
                Ok(None) => {
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Database error while locking forge outbox entry: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    async fn handle_failure(&self, id: i64, retry_count: i64, err: &str) {
        if Self::is_permanent_failure(err) || retry_count + 1 >= self.max_retries as i64 {
            if let Err(db_err) = self.db.mark_forge_outbox_failed(id, err).await {
                error!("Failed to mark forge outbox {} as failed: {}", id, db_err);
            }
        } else {
            let delay = Self::backoff_seconds(retry_count);
            let retry_at = chrono::Utc::now().timestamp() + delay;
            if let Err(db_err) = self.db.set_forge_outbox_retry_at(id, retry_at, err).await {
                error!(
                    "Failed to schedule retry for forge outbox {}: {}",
                    id, db_err
                );
            }
        }
    }
}
