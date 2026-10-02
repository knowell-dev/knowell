//! Shared helpers for unit tests. Everything here is synthetic.

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use uuid::Uuid;

use crate::ids::{
    ClientId, CommitId, RecordId, SessionId, Subject, SymbolId, Timestamp, UserId, ViewId,
};
use crate::model::{Actor, Evidence, NewRecord, RecordKind, Scope};

pub(crate) fn ts(n: i64) -> Timestamp {
    Timestamp::from_unix_seconds(n)
}

pub(crate) fn rid(n: u128) -> RecordId {
    RecordId::from_uuid(Uuid::from_u128(n))
}

pub(crate) fn human(id: &str) -> Actor {
    Actor::Human(UserId::new(id).unwrap())
}

pub(crate) fn agent(session: &str) -> Actor {
    Actor::Agent {
        session: SessionId::new(session).unwrap(),
        client: ClientId::new("testclient").unwrap(),
    }
}

pub(crate) fn sym(s: &str) -> SymbolId {
    SymbolId::new(s).unwrap()
}

pub(crate) fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub(crate) fn project_scope(project: &str) -> Scope {
    Scope::Project {
        workspace: name("shop"),
        project: name(project),
    }
}

/// Hash derived from a label, so tests can say "h1" and "h2".
pub(crate) fn hash(label: &str) -> ContentHash {
    ContentHash::of(label.as_bytes())
}

pub(crate) fn evidence_in(project: &str, path: &str, hash_label: &str) -> Evidence {
    Evidence {
        project: name(project),
        view: ViewId::new("main").unwrap(),
        commit: CommitId::new("abc1234").unwrap(),
        path: RepoPath::new(path).unwrap(),
        range: LineRange::new(10, 20).unwrap(),
        content_hash: hash(hash_label),
    }
}

pub(crate) fn evidence(path: &str, hash_label: &str) -> Evidence {
    evidence_in("api", path, hash_label)
}

pub(crate) fn new_record(kind: RecordKind, n: u128) -> NewRecord {
    NewRecord {
        id: rid(n),
        scope: project_scope("api"),
        kind,
        subject: Subject::new("payments.idempotency").unwrap(),
        title: format!("Record {n}"),
        body: "Body text for the record.".to_string(),
        evidence: vec![evidence("src/pay.rs", "h1")],
        related_symbols: Vec::new(),
        tags: Vec::new(),
        pinned: false,
    }
}

/// A fake GitHub-style token assembled at runtime; matches the scanner's
/// shape but is not a credential.
pub(crate) fn fake_token() -> String {
    format!("ghp_{}", "FAKE".repeat(9))
}
