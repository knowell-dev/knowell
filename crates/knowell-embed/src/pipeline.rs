//! The provider-independent part of an embedding call: validation, size
//! checks, batching, budget reservation, result checking and usage totals.

use std::ops::Range;
use std::time::Duration;

use crate::budget::Budget;
use crate::embedding::{DocumentInput, Embedding};
use crate::error::EmbedError;
use crate::limits::{BatchLimits, estimate_tokens};
use crate::profile::{ProviderKind, prepare_document, prepare_query};

/// What one embedding call cost, for the panel and for budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Input tokens: the provider's reported count when available,
    /// otherwise the conservative estimate (see `tokens_estimated`).
    pub input_tokens: u64,
    /// `true` when at least one batch had no provider-reported token count
    /// and its estimate was used instead.
    pub tokens_estimated: bool,
    /// Requests sent (batches), not counting retries.
    pub requests: u32,
    /// Extra attempts caused by 429, 5xx, timeouts or connection failures.
    pub retries: u32,
    /// Wall-clock time spent in HTTP exchanges, including backoff waits.
    pub latency: Duration,
}

impl Usage {
    /// Adds `other` into `self`.
    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.tokens_estimated |= other.tokens_estimated;
        self.requests = self.requests.saturating_add(other.requests);
        self.retries = self.retries.saturating_add(other.retries);
        self.latency = self.latency.saturating_add(other.latency);
    }
}

/// Embeddings (same order as the inputs) plus what producing them cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
    /// One normalised vector per input, in input order.
    pub embeddings: Vec<Embedding>,
    /// Cost of the call.
    pub usage: Usage,
}

/// Raw result of one provider request.
#[derive(Debug)]
pub(crate) struct BatchOutput {
    pub(crate) vectors: Vec<Vec<f32>>,
    /// Provider-reported input tokens, if any.
    pub(crate) tokens: Option<u64>,
    pub(crate) attempts: u32,
    pub(crate) latency: Duration,
}

/// One provider's way of embedding a single batch of prepared texts.
pub(crate) trait BatchSender: Sync {
    fn kind(&self) -> ProviderKind;
    fn dimensions(&self) -> usize;
    fn batch_limits(&self) -> &BatchLimits;
    fn budget(&self) -> Option<&Budget>;
    fn send(
        &self,
        texts: &[String],
        estimated_tokens: u64,
    ) -> impl Future<Output = Result<BatchOutput, EmbedError>> + Send;
}

/// Splits inputs (by their token estimates) into consecutive ranges that
/// respect both the entry and the token cap. An input is never split or
/// dropped; callers have already refused inputs above `max_input_tokens`,
/// which `BatchLimits::validate` guarantees fits an empty batch.
pub(crate) fn plan_batches(estimates: &[u64], limits: &BatchLimits) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut tokens = 0u64;
    for (i, est) in estimates.iter().enumerate() {
        let entries = i - start;
        if entries > 0
            && (entries >= limits.max_entries
                || tokens.saturating_add(*est) > limits.max_batch_tokens)
        {
            ranges.push(start..i);
            start = i;
            tokens = 0;
        }
        tokens = tokens.saturating_add(*est);
    }
    if start < estimates.len() {
        ranges.push(start..estimates.len());
    }
    ranges
}

/// Rejects documents the providers cannot meaningfully embed.
fn validate_documents(docs: &[DocumentInput]) -> Result<(), EmbedError> {
    for (index, doc) in docs.iter().enumerate() {
        if doc.text.trim().is_empty() {
            return Err(EmbedError::InvalidInput(format!(
                "document {index} has empty text"
            )));
        }
    }
    Ok(())
}

/// Embeds documents through `sender`.
pub(crate) async fn documents_via<S: BatchSender>(
    sender: &S,
    documents: &[DocumentInput],
) -> Result<Embedded, EmbedError> {
    validate_documents(documents)?;
    let texts = documents
        .iter()
        .map(|d| prepare_document(sender.kind(), d))
        .collect();
    embed_texts(sender, texts).await
}

/// Embeds one search query through `sender`.
pub(crate) async fn query_via<S: BatchSender>(
    sender: &S,
    query: &str,
) -> Result<(Embedding, Usage), EmbedError> {
    if query.trim().is_empty() {
        return Err(EmbedError::InvalidInput("the query is empty".into()));
    }
    let result = embed_texts(sender, vec![prepare_query(sender.kind(), query)]).await?;
    let usage = result.usage;
    match result.embeddings.into_iter().next() {
        Some(embedding) => Ok((embedding, usage)),
        None => Err(EmbedError::Response {
            provider: sender.kind().as_str(),
            message: "no embedding returned for the query".into(),
        }),
    }
}

/// Runs the whole call: size checks, budget reservation, batching, result
/// validation and normalisation.
async fn embed_texts<S: BatchSender>(
    sender: &S,
    texts: Vec<String>,
) -> Result<Embedded, EmbedError> {
    let provider = sender.kind().as_str();
    let dimensions = sender.dimensions();
    let limits = sender.batch_limits();
    let budget = sender.budget();
    if texts.is_empty() {
        return Ok(Embedded {
            embeddings: Vec::new(),
            usage: Usage::default(),
        });
    }
    let estimates: Vec<u64> = texts.iter().map(|t| estimate_tokens(t)).collect();
    for (index, est) in estimates.iter().enumerate() {
        if *est > limits.max_input_tokens {
            return Err(EmbedError::InputTooLong {
                index,
                estimated_tokens: *est,
                limit: limits.max_input_tokens,
            });
        }
    }
    let total: u64 = estimates.iter().fold(0u64, |a, b| a.saturating_add(*b));
    // Refuse the whole call up front rather than failing half-way through.
    let mut reservation = match budget {
        Some(b) => Some(b.reserve(total)?),
        None => None,
    };

    let mut embeddings = Vec::with_capacity(texts.len());
    let mut usage = Usage::default();
    for range in plan_batches(&estimates, limits) {
        let (Some(chunk), Some(chunk_estimates)) = (texts.get(range.clone()), estimates.get(range))
        else {
            return Err(EmbedError::Response {
                provider,
                message: "internal batching error".into(),
            });
        };
        let estimated: u64 = chunk_estimates.iter().sum();
        let output = sender.send(chunk, estimated).await?;
        if output.vectors.len() != chunk.len() {
            return Err(EmbedError::Response {
                provider,
                message: format!(
                    "returned {} embeddings for {} inputs",
                    output.vectors.len(),
                    chunk.len()
                ),
            });
        }
        for values in output.vectors {
            if values.len() != dimensions {
                return Err(EmbedError::Response {
                    provider,
                    message: format!(
                        "returned {} dimensions but the profile expects {dimensions}",
                        values.len()
                    ),
                });
            }
            let embedding = Embedding::from_values(values).map_err(|e| EmbedError::Response {
                provider,
                message: e.to_string(),
            })?;
            embeddings.push(embedding);
        }
        let tokens = match output.tokens {
            Some(t) => t,
            None => {
                usage.tokens_estimated = true;
                estimated
            }
        };
        if let Some(r) = reservation.as_mut() {
            r.add_actual(tokens);
        }
        usage.input_tokens = usage.input_tokens.saturating_add(tokens);
        usage.requests = usage.requests.saturating_add(1);
        usage.retries = usage
            .retries
            .saturating_add(output.attempts.saturating_sub(1));
        usage.latency = usage.latency.saturating_add(output.latency);
    }
    Ok(Embedded { embeddings, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(entries: usize, batch: u64, input: u64) -> BatchLimits {
        BatchLimits {
            max_entries: entries,
            max_batch_tokens: batch,
            max_input_tokens: input,
        }
    }

    #[test]
    fn splits_by_entry_count() {
        let est = vec![1u64; 250];
        let plan = plan_batches(&est, &limits(100, 1_000_000, 10));
        assert_eq!(plan, vec![0..100, 100..200, 200..250]);
    }

    #[test]
    fn splits_by_token_cap_and_keeps_every_input() {
        let est = vec![40u64, 40, 40, 90, 10];
        let plan = plan_batches(&est, &limits(100, 100, 90));
        assert_eq!(plan, vec![0..2, 2..3, 3..5]);
        let covered: usize = plan.iter().map(|r| r.len()).sum();
        assert_eq!(covered, est.len());
    }

    #[test]
    fn empty_input_plans_nothing() {
        assert!(plan_batches(&[], &limits(10, 10, 10)).is_empty());
    }

    #[test]
    fn validates_documents() {
        assert!(validate_documents(&[DocumentInput::new("ok")]).is_ok());
        assert!(
            validate_documents(&[DocumentInput::new("ok"), DocumentInput::new("  \n")]).is_err()
        );
    }

    #[test]
    fn usage_merges() {
        let mut a = Usage {
            input_tokens: 5,
            requests: 1,
            retries: 1,
            latency: Duration::from_millis(10),
            tokens_estimated: false,
        };
        a.merge(&Usage {
            input_tokens: 7,
            requests: 2,
            retries: 0,
            latency: Duration::from_millis(5),
            tokens_estimated: true,
        });
        assert_eq!(a.input_tokens, 12);
        assert_eq!(a.requests, 3);
        assert!(a.tokens_estimated);
        assert_eq!(a.latency, Duration::from_millis(15));
    }
}
