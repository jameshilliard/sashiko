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

//! Security and normalization helpers shared across review workflows.

/// Verifies that a guide name selected by an LLM (e.g. during pre-screening) is
/// a plain filename without path separators or parent directory components.
///
/// Selected guide names are joined against the prompt bundle directory and
/// inlined into stage system prompts. Since patches from untrusted sources can
/// influence LLM selection, any name containing path traversal characters is
/// rejected.
pub fn sanitize_guide_name(name: &str) -> bool {
    let plain =
        !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..");
    if !plain {
        tracing::warn!("Ignoring prescreen guide with a path in its name: {name}");
    }
    plain
}

/// Normalizes a stage name returned by an LLM planner or CLI flag to its
/// canonical lowercase kebab-case identifier without leading `stage-` prefix.
pub fn normalize_stage_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase().replace('_', "-");
    if let Some(stripped) = lower.strip_prefix("stage-") {
        stripped.to_string()
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_guide_name_accepts_plain_filename() {
        assert!(sanitize_guide_name("locking.md"));
        assert!(sanitize_guide_name("workflow-engine.md"));
        assert!(sanitize_guide_name("rust-async.md"));
    }

    #[test]
    fn test_sanitize_guide_name_rejects_traversal_and_paths() {
        assert!(!sanitize_guide_name(""));
        assert!(!sanitize_guide_name("../etc/passwd"));
        assert!(!sanitize_guide_name("subsystem/locking.md"));
        assert!(!sanitize_guide_name("subsystem\\locking.md"));
        assert!(!sanitize_guide_name(".."));
    }

    #[test]
    fn test_normalize_stage_name() {
        assert_eq!(
            normalize_stage_name("Stage_Execution_Flow"),
            "execution-flow"
        );
        assert_eq!(normalize_stage_name("stage-llm-pipeline"), "llm-pipeline");
        assert_eq!(normalize_stage_name("  CONCURRENCY "), "concurrency");
    }
}
