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

use crate::ai::truncator::Truncator;
use crate::toolbox::SashikoToolContext;
use crate::toolbox::command::capped_output;
use crate::toolbox::framework::LlmTool;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct GitLogTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitLogTool {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn description(&self) -> &'static str {
        "Show commit logs in a specific range or revision history."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "range": { "type": "string", "description": "The commit range or reference to view logs for (e.g., 'baseline..HEAD' or 'HEAD')." },
                "limit": { "type": "integer", "description": "Limit the number of commits returned (defaults to 10, max 100)." }
            },
            "required": ["range"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut()
            && !obj.contains_key("limit")
        {
            obj.insert("limit".to_string(), json!(10));
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let range_raw = args["range"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing range"))?;
        let range_virt = context.virtualize_ref(range_raw);
        let range = range_virt.as_str();
        let limit = args["limit"].as_u64().unwrap_or(10).min(100) as usize;

        if range.starts_with('-') {
            return Err(anyhow!("Invalid range"));
        }

        let limit_str = limit.to_string();
        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["log", "-n", &limit_str, range])
            .kill_on_drop(true);

        let output = capped_output(&mut cmd).await?;
        if !output.is_usable() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(json!({ "error": format!("git log failed: {}", stderr) }));
        }

        let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let total_log_lines = raw_stdout.lines().count();
        let res = Truncator::truncate_sequential(&raw_stdout, 10_000);
        let truncated_log = res.content;
        let lines_kept = res.lines_kept;
        let is_truncated = res.truncated;

        let returned_items = if is_truncated && lines_kept > 0 {
            lines_kept
        } else {
            total_log_lines
        };

        Ok(json!({
            "content": truncated_log,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_log_lines,
                "returned_items": returned_items
            },
            "next_page_hint": if is_truncated {
                Some("The log output was truncated. Use a smaller commit range or set a lower 'limit' parameter.".to_string())
            } else {
                None
            },
            "output": truncated_log
        }))
    }
}
