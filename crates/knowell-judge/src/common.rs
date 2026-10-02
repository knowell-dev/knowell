//! Request preparation and result assembly shared by every reranker.

use std::time::Duration;

use url::{Host, Url};

use crate::{Candidate, JudgeError, Scored};

/// Limits and resilience settings shared by the HTTP providers.
///
/// The defaults suit a hosted API; lower `max_documents` for a local TEI
/// server (its `--max-client-batch-size` defaults to 32).
#[derive(Clone, Debug)]
pub struct HttpOptions {
    /// Total time allowed for one HTTP attempt. Default 30 s.
    pub timeout: Duration,
    /// Time allowed to establish the connection. Default 5 s.
    pub connect_timeout: Duration,
    /// Retries after the first attempt on HTTP 429 and 5xx. Default 2.
    pub max_retries: u32,
    /// Wait before the first retry; doubles each retry. Default 200 ms.
    pub initial_backoff: Duration,
    /// Upper bound of the exponential backoff. Default 5 s.
    pub max_backoff: Duration,
    /// Longest `Retry-After` that is honoured by waiting. A longer request
    /// ends the call with [`JudgeError::RateLimited`] instead of stalling a
    /// search for minutes. Default 30 s.
    pub max_retry_after: Duration,
    /// Simultaneous in-flight requests per provider instance. Default 4.
    pub max_concurrency: usize,
    /// Most candidates accepted per call; more is an error, never a silent
    /// split or drop. Default 1000 (Voyage's hard limit, Cohere's advice).
    pub max_documents: usize,
    /// Per-document truncation in Unicode scalar values (not bytes), applied
    /// before sending. Default 8000, roughly 2000 tokens of code.
    pub max_document_chars: usize,
    /// Query truncation in Unicode scalar values. Default 2000.
    pub max_query_chars: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            max_retries: 2,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            max_retry_after: Duration::from_secs(30),
            max_concurrency: 4,
            max_documents: 1000,
            max_document_chars: 8000,
            max_query_chars: 2000,
        }
    }
}

impl HttpOptions {
    pub(crate) fn validate(&self) -> Result<(), JudgeError> {
        if self.max_concurrency == 0 {
            return Err(bad_config("max_concurrency must be at least 1"));
        }
        if self.max_documents == 0 {
            return Err(bad_config("max_documents must be at least 1"));
        }
        if self.max_document_chars == 0 || self.max_query_chars == 0 {
            return Err(bad_config("character limits must be at least 1"));
        }
        if self.timeout.is_zero() || self.connect_timeout.is_zero() {
            return Err(bad_config("timeouts must be greater than zero"));
        }
        Ok(())
    }
}

pub(crate) fn bad_config(msg: &str) -> JudgeError {
    JudgeError::InvalidConfig(msg.to_owned())
}

/// Validated, truncated inputs ready to be put on the wire.
pub(crate) struct Prepared {
    pub(crate) query: String,
    pub(crate) texts: Vec<String>,
    /// `top_n` clamped to the candidate count.
    pub(crate) top_n: usize,
}

/// Validates a call. Returns `None` when there is nothing to score.
pub(crate) fn prepare(
    opts: &HttpOptions,
    query: &str,
    candidates: &[Candidate],
    top_n: usize,
) -> Result<Option<Prepared>, JudgeError> {
    if top_n == 0 {
        return Err(JudgeError::InvalidRequest(
            "top_n must be at least 1".into(),
        ));
    }
    if query.trim().is_empty() {
        return Err(JudgeError::InvalidRequest("query is empty".into()));
    }
    if candidates.len() > opts.max_documents {
        return Err(JudgeError::TooManyCandidates {
            given: candidates.len(),
            max: opts.max_documents,
        });
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    Ok(Some(Prepared {
        query: truncate_chars(query, opts.max_query_chars).to_owned(),
        texts: candidates
            .iter()
            .map(|c| truncate_chars(&c.text, opts.max_document_chars).to_owned())
            .collect(),
        top_n: top_n.min(candidates.len()),
    }))
}

/// Cuts `s` to at most `max` characters, always on a character boundary.
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte, _)) => s.get(..byte).unwrap_or(s),
        None => s,
    }
}

/// Validates provider hits (`(candidate index, score)`), orders them best
/// first with the input order as tie-break, and maps them to candidate ids.
pub(crate) fn finish(
    candidates: &[Candidate],
    hits: Vec<(usize, f32)>,
    top_n: usize,
) -> Result<Vec<Scored>, JudgeError> {
    if hits.is_empty() {
        return Err(JudgeError::InvalidResponse(
            "provider returned no results".into(),
        ));
    }
    let mut seen = vec![false; candidates.len()];
    for (index, score) in &hits {
        let slot = seen.get_mut(*index).ok_or_else(|| {
            JudgeError::InvalidResponse(format!("result index {index} is out of range"))
        })?;
        if *slot {
            return Err(JudgeError::InvalidResponse(format!(
                "result index {index} appears twice"
            )));
        }
        *slot = true;
        if !score.is_finite() {
            return Err(JudgeError::InvalidResponse(
                "score is not a finite number".into(),
            ));
        }
    }
    let mut hits = hits;
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    hits.truncate(top_n);
    Ok(hits
        .into_iter()
        .filter_map(|(index, score)| {
            candidates.get(index).map(|c| Scored {
                id: c.id.clone(),
                score,
            })
        })
        .collect())
}

/// Builds an endpoint URL by appending `path` to `base`, rejecting URLs that
/// could leak secrets (embedded credentials, query strings, fragments).
pub(crate) fn endpoint(base: &Url, path: &str) -> Result<Url, JudgeError> {
    if !matches!(base.scheme(), "http" | "https") {
        return Err(bad_config("base_url must use http or https"));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(bad_config("base_url must not contain credentials"));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(bad_config("base_url must not contain a query or fragment"));
    }
    let mut url = base.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| bad_config("base_url cannot be used as a base"))?;
        segments.pop_if_empty();
        segments.extend(path.split('/').filter(|s| !s.is_empty()));
    }
    Ok(url)
}

/// Whether the URL points at this machine (localhost or a loopback address).
pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(n: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| Candidate::new(format!("c{i}"), format!("text {i}")))
            .collect()
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("日本語", 1), "日");
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn prepare_validates() {
        let o = HttpOptions {
            max_documents: 2,
            ..HttpOptions::default()
        };
        assert!(matches!(
            prepare(&o, "q", &cands(1), 0),
            Err(JudgeError::InvalidRequest(_))
        ));
        assert!(matches!(
            prepare(&o, "  ", &cands(1), 1),
            Err(JudgeError::InvalidRequest(_))
        ));
        assert!(matches!(
            prepare(&o, "q", &cands(3), 1),
            Err(JudgeError::TooManyCandidates { given: 3, max: 2 })
        ));
        assert!(matches!(prepare(&o, "q", &[], 1), Ok(None)));
        let p = prepare(&o, "q", &cands(2), 9).ok().flatten();
        assert_eq!(p.map(|p| p.top_n), Some(2));
    }

    #[test]
    fn finish_orders_and_breaks_ties_by_input_order() {
        let c = cands(3);
        let out = finish(&c, vec![(2, 0.5), (0, 0.5), (1, 0.9)], 3).unwrap();
        let ids: Vec<_> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["c1", "c0", "c2"]);
        let top1 = finish(&c, vec![(2, 0.5), (0, 0.5), (1, 0.9)], 1).unwrap();
        assert_eq!(top1.len(), 1);
    }

    #[test]
    fn finish_rejects_bad_hits() {
        let c = cands(2);
        assert!(finish(&c, vec![], 1).is_err());
        assert!(finish(&c, vec![(5, 1.0)], 1).is_err());
        assert!(finish(&c, vec![(0, 1.0), (0, 0.5)], 2).is_err());
        assert!(finish(&c, vec![(0, f32::NAN)], 1).is_err());
        assert!(finish(&c, vec![(0, f32::INFINITY)], 1).is_err());
    }

    #[test]
    fn endpoint_joins_and_rejects_leaky_urls() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            endpoint(&u("https://h.example/v1/"), "rerank")
                .unwrap()
                .as_str(),
            "https://h.example/v1/rerank"
        );
        assert_eq!(
            endpoint(&u("https://h.example"), "v2/rerank")
                .unwrap()
                .as_str(),
            "https://h.example/v2/rerank"
        );
        assert!(endpoint(&u("https://user:pw@h.example"), "x").is_err());
        assert!(endpoint(&u("https://h.example/?key=abc"), "x").is_err());
        assert!(endpoint(&u("ftp://h.example"), "x").is_err());
    }

    #[test]
    fn loopback_detection() {
        for (s, want) in [
            ("http://localhost:8080", true),
            ("http://127.0.0.1", true),
            ("http://[::1]:1", true),
            ("http://10.0.0.5", false),
            ("https://api.example.com", false),
        ] {
            assert_eq!(
                Url::parse(s).map(|u| is_loopback(&u)).ok(),
                Some(want),
                "{s}"
            );
        }
    }
}
