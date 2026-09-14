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

use crate::patch::{Patch, PatchsetMetadata};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSource {
    Nntp,
    ApiInject,
    ApiFetchThread,
    GitFetch,
    GitImport,
    GitArchive,
}

#[derive(Debug, Default)]
struct TrackerState {
    /// Articles handed downstream whose fate is not yet known.
    in_flight: usize,
    /// Lowest article that was lost rather than settled. The mark must stay
    /// below it so the next pass fetches it again.
    lowest_lost: Option<u64>,
}

#[derive(Debug, Default)]
struct TrackerInner {
    state: Mutex<TrackerState>,
    settled: tokio::sync::Notify,
}

/// Follows articles from the fetch loop to the point where they are stored.
///
/// The high-water mark records that a group has been read up to some article,
/// and the fetch loop never looks below it again. Moving it when an article is
/// merely queued is therefore a one way door: anything still in memory when the
/// process dies is lost for good, because nothing will fetch it a second time.
///
/// Articles are carried by a receipt instead. Holding the mark until every
/// receipt of a batch has come back turns that permanent loss into a refetch,
/// which costs nothing because messages are stored by upsert.
#[derive(Debug, Default, Clone)]
pub struct IngestTracker {
    inner: Arc<TrackerInner>,
}

impl IngestTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a receipt for an article about to be handed downstream.
    pub fn issue(&self, article: u64) -> IngestReceipt {
        self.inner.state.lock().expect("tracker lock").in_flight += 1;
        IngestReceipt {
            inner: self.inner.clone(),
            article,
            settled: false,
        }
    }

    /// Waits until every issued receipt has come back.
    pub async fn wait_until_quiet(&self) {
        loop {
            // Registering before the check closes the gap where a receipt
            // could come back between the two and leave nobody to wake.
            let quiet = self.inner.settled.notified();
            if self.inner.state.lock().expect("tracker lock").in_flight == 0 {
                return;
            }
            quiet.await;
        }
    }

    /// The highest article the mark may move to, given everything that was
    /// lost, and clears the record so the next batch starts clean.
    ///
    /// Only meaningful once the tracker is quiet.
    pub fn take_safe_mark(&self, handed_off_up_to: u64) -> u64 {
        let mut state = self.inner.state.lock().expect("tracker lock");
        match state.lowest_lost.take() {
            Some(lost) => handed_off_up_to.min(lost.saturating_sub(1)),
            None => handed_off_up_to,
        }
    }
}

/// Reports the fate of one article back to the tracker.
///
/// Dropping a receipt without settling it reports the article as lost, which
/// covers the paths nobody remembers to write: a panicking task, a closed
/// channel, an early return. Losing an article only delays it, so the safe
/// behaviour is the one that costs nothing to forget.
#[derive(Debug)]
pub struct IngestReceipt {
    inner: Arc<TrackerInner>,
    article: u64,
    settled: bool,
}

impl IngestReceipt {
    /// Marks the article as dealt with for good, whether it was stored or
    /// deliberately rejected. Either way there is no point fetching it again.
    pub fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for IngestReceipt {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("tracker lock");
        state.in_flight -= 1;
        if !self.settled {
            state.lowest_lost = Some(match state.lowest_lost {
                Some(lowest) => lowest.min(self.article),
                None => self.article,
            });
        }
        if state.in_flight == 0 {
            self.inner.settled.notify_waiters();
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum Event {
    ArticleFetched {
        group: String,
        article_id: String,
        content: Vec<String>,
        raw: Option<Vec<u8>>,
        baseline: Option<String>,
        /// Present for articles read from NNTP, whose high-water mark may not
        /// move until this comes back. Other sources re-read from a source
        /// that still has the message, so they do not need one.
        receipt: Option<IngestReceipt>,
    },
    PatchSubmitted {
        group: String,
        article_id: String,
        message_id: String,
        subject: String,
        author: String,
        message: String,
        diff: String,
        base_commit: Option<String>,
        timestamp: i64,
        index: u32,
        total: u32,
        mr_url: Option<String>,
        mr_title: Option<String>,
        mr_number: Option<i64>,
    },
    RawMboxSubmitted {
        raw: String,
        submission_id: String,
        source: MessageSource,
        group: String,
        baseline: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
        /// Server-side timestamp for when the submission was received.
        /// Used instead of the email's Date: header for patchset ordering
        /// to prevent stale mbox timestamps from skewing the queue.
        submitted_at: Option<i64>,
    },
    IngestionFailed {
        article_id: String,
        error: String,
        source: MessageSource,
    },
}

#[derive(Debug)]
pub struct ParsedArticle {
    pub group: String,
    pub article_id: String,
    pub source: MessageSource,
    pub metadata: Option<PatchsetMetadata>,
    pub patch: Option<Patch>,
    pub baseline: Option<String>,
    pub failed_error: Option<String>,
    pub skip_filters: Option<Vec<String>>,
    pub only_filters: Option<Vec<String>>,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
    /// Travels with the article so that the fetch loop learns whether it was
    /// stored. See IngestTracker.
    pub receipt: Option<IngestReceipt>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_stops_below_the_lowest_article_that_was_lost() {
        let tracker = IngestTracker::new();

        let mut first = tracker.issue(1);
        let second = tracker.issue(2);
        let mut third = tracker.issue(3);

        first.settle();
        third.settle();
        drop(second);
        drop(first);
        drop(third);

        assert_eq!(tracker.take_safe_mark(3), 1);
    }

    #[test]
    fn the_mark_reaches_the_last_article_when_none_were_lost() {
        let tracker = IngestTracker::new();

        for article in 1..=3 {
            let mut receipt = tracker.issue(article);
            receipt.settle();
        }

        assert_eq!(tracker.take_safe_mark(3), 3);
    }

    #[test]
    fn a_lost_article_only_holds_the_mark_back_once() {
        let tracker = IngestTracker::new();

        drop(tracker.issue(5));
        assert_eq!(tracker.take_safe_mark(9), 4);

        // The next batch starts clean, so the loss is not counted twice.
        assert_eq!(tracker.take_safe_mark(19), 19);
    }

    #[tokio::test]
    async fn waiting_ends_once_the_last_receipt_comes_back() {
        let tracker = IngestTracker::new();
        let receipt = tracker.issue(1);

        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_until_quiet().await })
        };

        // The waiter cannot finish while the receipt is outstanding.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(receipt);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiting for quiet timed out")
            .expect("waiter panicked");
    }

    #[tokio::test]
    async fn waiting_returns_at_once_when_nothing_is_in_flight() {
        let tracker = IngestTracker::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tracker.wait_until_quiet(),
        )
        .await
        .expect("waiting for quiet timed out");
    }
}
