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

pub struct TokenBudget {
    pub max_tokens: usize,
    pub current: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            current: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.current)
    }

    pub fn can_afford(&self, estimated_tokens: usize) -> bool {
        self.current + estimated_tokens <= self.max_tokens
    }

    pub fn consume(&mut self, tokens: usize) {
        self.current += tokens;
    }

    pub fn reset(&mut self) {
        self.current = 0;
    }

    /// Bytes of text assumed to make up one token.
    ///
    /// Three is what the content these budgets are spent on actually measures:
    /// source code, diffs and lock files all sit near three bytes per token.
    /// Prose runs closer to four, so assuming three over-counts it and
    /// truncates a little early. That is the direction to be wrong in, because
    /// the opposite lets a tool result overrun the budget it was given.
    pub const BYTES_PER_TOKEN: usize = 3;

    /// Approximates the token count of a string from its byte length.
    ///
    /// This is deliberately arithmetic rather than a real encode. Sashiko
    /// talks to several providers and each has its own vocabulary, so a count
    /// produced by any single tokenizer is an approximation of the model
    /// actually in use no matter how exact that tokenizer is. Callers spend
    /// the number on context budgets that are orders of magnitude larger than
    /// the error, while a real encode over a large tool output costs enough
    /// CPU to stall the async runtime that asked for it.
    pub fn approximate_tokens(text: &str) -> usize {
        text.len().div_ceil(Self::BYTES_PER_TOKEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_management() {
        let mut budget = TokenBudget::new(100);
        assert_eq!(budget.remaining(), 100);

        budget.consume(20);
        assert_eq!(budget.remaining(), 80);
        assert_eq!(budget.current, 20);

        assert!(budget.can_afford(10));
        assert!(!budget.can_afford(90));
    }

    #[test]
    fn test_approximate_tokens() {
        assert_eq!(TokenBudget::approximate_tokens(""), 0);

        // Rounding up keeps anything non-empty from being free.
        assert_eq!(TokenBudget::approximate_tokens("a"), 1);
        assert_eq!(TokenBudget::approximate_tokens("abc"), 1);
        assert_eq!(TokenBudget::approximate_tokens("abcd"), 2);

        let short = TokenBudget::approximate_tokens("hello");
        let longer = TokenBudget::approximate_tokens("hello world");
        assert!(longer > short);
    }

    #[test]
    fn test_approximate_tokens_does_not_undercount_real_text() {
        // Budgets are only safe if the estimate errs high: a count below the
        // true one lets a tool result overrun the context it was given.
        // English prose is the worst case, sitting near four bytes per token.
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(100);
        let generous_true_count = prose.len() / 4;

        assert!(
            TokenBudget::approximate_tokens(&prose) >= generous_true_count,
            "estimate must not fall below a four-bytes-per-token reading"
        );
    }

    #[test]
    fn test_approximate_tokens_is_cheap_on_large_input() {
        // The encode this replaced took minutes on input of this size, which
        // is what wedged a runtime worker in production.
        let content =
            "drivers/gpu/drm/xe/xe_tlb_inval.c:42: xe_tlb_inval_issue(inval);\n".repeat(200_000);

        let start = std::time::Instant::now();
        let tokens = TokenBudget::approximate_tokens(&content);
        let duration = start.elapsed();

        assert!(tokens > 0);
        assert!(
            duration < std::time::Duration::from_millis(100),
            "estimating {} bytes took {:?}",
            content.len(),
            duration
        );
    }
}
