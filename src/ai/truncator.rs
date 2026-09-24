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

//! Helpers that cut oversized tool output down to a context budget.
//!
//! Truncation runs on whatever a tool produced, which for a repository-wide
//! git command can be hundreds of megabytes. Every operation here is
//! therefore linear in the input and allocates at most one buffer the size of
//! the budget. In particular the module measures content with
//! [`TokenBudget::approximate_tokens`] rather than a real tokenizer: encoding
//! a candidate on each step of a search turned a single large tool result
//! into minutes of uninterruptible CPU on a runtime worker.

use crate::ai::token_budget::TokenBudget;

pub struct Truncator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequentialTruncationResult {
    pub content: String,
    pub lines_kept: usize,
    pub truncated: bool,
}

impl Truncator {
    /// Truncates a diff output if it's too large.
    /// Preserves the header and checks for balanced chunks.
    pub fn truncate_diff(diff: &str, max_tokens: usize, label: &str) -> TruncationResult {
        let estimated = TokenBudget::approximate_tokens(diff);
        if estimated <= max_tokens {
            return TruncationResult {
                content: diff.to_string(),
                truncated: false,
            };
        }

        let budget_bytes = max_tokens * TokenBudget::BYTES_PER_TOKEN;
        let lines: Vec<&str> = diff.lines().collect();
        let total_lines = lines.len();

        // Heuristic: If total lines is small but content is huge, we have long lines.
        // We calculate 'allowed_lines' based on a conservative average line length (e.g. 50 chars).
        let allowed_lines = budget_bytes / 50;

        if total_lines <= allowed_lines {
            // Too few lines to drop any, yet over budget, so the lines
            // themselves are huge. Nothing can be cut on a line boundary.
            return TruncationResult {
                content: Self::head_of(diff, max_tokens, estimated),
                truncated: true,
            };
        }

        let keep_top = allowed_lines / 2;
        let keep_bottom = allowed_lines / 2;

        if keep_top + keep_bottom >= total_lines {
            // Should be covered by above check, but safety fallback
            return TruncationResult {
                content: Self::head_of(diff, max_tokens, estimated),
                truncated: true,
            };
        }

        let mut result = String::new();
        for line in &lines[..keep_top] {
            result.push_str(line);
            result.push('\n');
        }

        result.push_str(&format!(
            "\n... [{} truncated. Dropped {} lines (lines {}-{})] ...\n\n",
            label,
            total_lines - (keep_top + keep_bottom),
            keep_top + 1,
            total_lines - keep_bottom
        ));

        for line in &lines[total_lines - keep_bottom..] {
            result.push_str(line);
            result.push('\n');
        }

        // Final safety net: the kept head and tail, plus the notice between
        // them, can still come to more than the budget allows.
        if TokenBudget::approximate_tokens(&result) > max_tokens {
            return TruncationResult {
                content: Self::head_of(&result, max_tokens, estimated),
                truncated: true,
            };
        }

        TruncationResult {
            content: result,
            truncated: true,
        }
    }

    /// Sequentially truncates content, keeping only the leading lines that fit
    /// the budget. Appends a truncation warning.
    ///
    /// Runs in one pass over the input and allocates a single buffer bounded
    /// by `max_tokens`, so the cost does not grow with how far the content
    /// overshoots the budget.
    pub fn truncate_sequential(content: &str, max_tokens: usize) -> SequentialTruncationResult {
        let estimated = TokenBudget::approximate_tokens(content);
        if estimated <= max_tokens {
            return SequentialTruncationResult {
                content: content.to_string(),
                lines_kept: content.lines().count(),
                truncated: false,
            };
        }

        let budget_bytes = max_tokens * TokenBudget::BYTES_PER_TOKEN;
        let total_lines = content.lines().count();

        // The warning is part of the returned content, so reserve room for it
        // before choosing how many lines to keep. Formatting it with the total
        // line count gives an upper bound, because the real dropped count is
        // never wider than the total.
        let reserved = Self::dropped_lines_warning(total_lines, estimated).len();
        let available = budget_bytes.saturating_sub(reserved);

        let mut result = String::with_capacity(budget_bytes);
        let mut lines_kept = 0;
        for line in content.lines() {
            if result.len() + line.len() + 1 > available {
                break;
            }
            result.push_str(line);
            result.push('\n');
            lines_kept += 1;
        }

        // A single line wider than the whole budget leaves nothing to keep.
        if lines_kept == 0 {
            return SequentialTruncationResult {
                content: Self::head_of(content, max_tokens, estimated),
                lines_kept: 0,
                truncated: true,
            };
        }

        result.push_str(&Self::dropped_lines_warning(
            total_lines - lines_kept,
            estimated,
        ));

        SequentialTruncationResult {
            content: result,
            lines_kept,
            truncated: true,
        }
    }

    /// The notice appended to content that lost whole lines.
    fn dropped_lines_warning(dropped_lines: usize, estimated_tokens: usize) -> String {
        format!(
            "... [Output truncated. Dropped {} lines. Original size: {} tokens] ...\n",
            dropped_lines, estimated_tokens
        )
    }

    /// Keeps the leading characters of content that cannot be split on a line
    /// boundary, such as a single very long line.
    ///
    /// The notice is part of the budget, so a budget too small to hold one
    /// yields nothing but the notice itself.
    fn head_of(content: &str, max_tokens: usize, estimated_tokens: usize) -> String {
        let budget_bytes = max_tokens * TokenBudget::BYTES_PER_TOKEN;
        // Sizing the reserve with the whole budget gives an upper bound on the
        // notice, because the count it reports can only be smaller.
        let reserved = Self::oversized_notice(estimated_tokens, budget_bytes).len();
        let allowed = budget_bytes.saturating_sub(reserved);

        let end = content
            .char_indices()
            .map(|(offset, c)| offset + c.len_utf8())
            .take_while(|end| *end <= allowed)
            .last()
            .unwrap_or(0);

        format!(
            "{}{}",
            &content[..end],
            Self::oversized_notice(estimated_tokens, end)
        )
    }

    /// The notice appended to content that had to be cut mid-line.
    fn oversized_notice(estimated_tokens: usize, kept_bytes: usize) -> String {
        format!(
            "\n... [Output truncated. Content too large ({} tokens). Displaying first {} bytes] ...\n",
            estimated_tokens, kept_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_diff_long_line_respects_the_byte_budget() {
        // One line far wider than the budget, so there is no line boundary to
        // cut on and the result comes from the mid-line path.
        let diff = format!("+{}", "x".repeat(50_000));

        let res = Truncator::truncate_diff(&diff, 200, "Diff");

        assert!(res.truncated);
        assert!(res.content.len() <= 200 * TokenBudget::BYTES_PER_TOKEN);
    }

    #[test]
    fn test_truncate_diff_multibyte_stays_within_the_byte_budget() {
        // The budget is in bytes, so counting the kept prefix in characters
        // overruns it by the encoded width of whatever is kept: three times
        // over for this input, and four for a wider code point.
        let diff = format!("+{}", "→".repeat(50_000));
        let budget_bytes = 200 * TokenBudget::BYTES_PER_TOKEN;

        let res = Truncator::truncate_diff(&diff, 200, "Diff");

        assert!(res.truncated);
        assert!(
            res.content.len() <= budget_bytes,
            "produced {} bytes against a {} byte budget",
            res.content.len(),
            budget_bytes
        );
        // Cutting on a byte offset must not split a character.
        assert!(res.content.is_char_boundary(res.content.len()));
    }

    #[test]
    fn test_truncate_diff() {
        // The line-based path needs a diff wider than its per-line allowance,
        // so a handful of short lines would only ever reach the character
        // fallback.
        let diff = (1..=200)
            .map(|i| format!("+    some_changed_call(arg_{});", i))
            .collect::<Vec<_>>()
            .join("\n");
        let res = Truncator::truncate_diff(&diff, 200, "Diff");
        assert!(res.content.contains("Diff truncated"));
        assert!(res.truncated);
        assert!(res.content.contains("some_changed_call(arg_1)"));
        assert!(res.content.contains("some_changed_call(arg_200)"));
    }

    #[test]
    fn test_truncate_diff_long_line() {
        // 1000 chars "a", but max_tokens = 20 (60 bytes).
        // allowed_lines = 60/50 = 1, total_lines = 1, so this takes the
        // mid-line path.
        let long_line = "a".repeat(1000);
        let res = Truncator::truncate_diff(&long_line, 20, "Diff");

        // The notice counts against the budget, and at 60 bytes there is no
        // room for both it and any content, so the notice is all that comes
        // back. Reporting the truncation matters more than a few bytes of a
        // line nobody can read the rest of. Same rule as the sequential path.
        assert!(res.content.len() < 300);
        assert!(res.content.contains("Output truncated"));
        assert!(
            res.truncated,
            "Should be marked as truncated despite being a single line"
        );
    }

    #[test]
    fn test_truncate_diff_long_line_keeps_content_at_a_realistic_budget() {
        // Any budget a tool is actually given dwarfs the notice, so the head
        // of the line survives.
        let long_line = "a".repeat(100_000);
        let res = Truncator::truncate_diff(&long_line, 10_000, "Diff");

        assert!(res.truncated);
        assert!(res.content.starts_with("aaaa"));
        assert!(res.content.contains("Output truncated"));
        assert!(res.content.len() <= 10_000 * TokenBudget::BYTES_PER_TOKEN);
    }

    #[test]
    fn test_truncate_sequential() {
        let content = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let res = Truncator::truncate_sequential(&content, 50);
        assert!(res.content.contains("line 0"));
        assert!(res.content.contains("Output truncated. Dropped"));
        assert!(!res.content.contains("line 99"));
        assert!(res.truncated);
        assert!(res.lines_kept > 0);
        assert!(res.lines_kept < 100);
    }

    #[test]
    fn test_truncate_sequential_result_fits_the_budget() {
        let content = (0..5_000)
            .map(|i| format!("match {} in some/path/file.c: some matching text", i))
            .collect::<Vec<_>>()
            .join("\n");

        for max_tokens in [100, 1_000, 10_000] {
            let res = Truncator::truncate_sequential(&content, max_tokens);
            assert!(res.truncated, "budget {} should truncate", max_tokens);
            assert!(
                res.content.len() <= max_tokens * TokenBudget::BYTES_PER_TOKEN,
                "budget {} produced {} bytes",
                max_tokens,
                res.content.len()
            );
        }
    }

    #[test]
    fn test_truncate_sequential_budget_too_small_for_a_notice() {
        let content = (0..5_000)
            .map(|i| format!("match {} in some/path/file.c: some matching text", i))
            .collect::<Vec<_>>()
            .join("\n");

        // Nothing useful fits, but the result still has to stay small rather
        // than fall back to returning the input.
        let res = Truncator::truncate_sequential(&content, 4);

        assert!(res.truncated);
        assert!(
            res.content.len() < 256,
            "produced {} bytes",
            res.content.len()
        );
    }

    #[test]
    fn test_truncate_sequential_handles_oversized_input() {
        // A repository-wide grep can return output of this order. The previous
        // implementation re-tokenized a candidate on every step of a binary
        // search over these lines, which took minutes of CPU.
        let line = "drivers/gpu/drm/xe/xe_tlb_inval.c:42: xe_tlb_inval_issue(inval);";
        let content = std::iter::repeat_n(line, 200_000)
            .collect::<Vec<_>>()
            .join("\n");

        let res = Truncator::truncate_sequential(&content, 10_000);

        assert!(res.truncated);
        assert!(res.lines_kept > 0);
        assert!(res.lines_kept < 200_000);
        assert!(res.content.len() <= 10_000 * TokenBudget::BYTES_PER_TOKEN);
        assert!(res.content.contains("Output truncated. Dropped"));
    }

    #[test]
    fn test_truncate_sequential_single_line_wider_than_budget() {
        let content = "x".repeat(10_000);
        let res = Truncator::truncate_sequential(&content, 200);

        assert!(res.truncated);
        assert_eq!(res.lines_kept, 0);
        assert!(res.content.starts_with("xxxx"));
        assert!(res.content.contains("Content too large"));
        assert!(res.content.len() <= 200 * TokenBudget::BYTES_PER_TOKEN);
    }

    #[test]
    fn test_truncate_diff_precise_range() {
        let diff = (1..=20)
            .map(|i| format!("diff line {} padding text", i))
            .collect::<Vec<_>>()
            .join("\n");
        // budget 80 tokens -> allowed_lines = (80 * 3) / 50 = 4 lines.
        // keep_top = 2, keep_bottom = 2. Total 20 lines.
        // Dropped lines count: 16. Range: 3 to 18.
        let res = Truncator::truncate_diff(&diff, 80, "Diff");
        assert!(res.truncated);
        assert!(
            res.content
                .contains("Diff truncated. Dropped 16 lines (lines 3-18)")
        );
        assert!(res.content.contains("diff line 1 "));
        assert!(res.content.contains("diff line 2 "));
        assert!(res.content.contains("diff line 19 "));
        assert!(res.content.contains("diff line 20"));
    }
}
