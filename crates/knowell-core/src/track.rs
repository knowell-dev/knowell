use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The ref a project view follows.
///
/// Knowell never assumes a branch name: a project without an explicit
/// target (directly or inherited from its workspace) is a configuration
/// error, and a target that does not exist is reported, never replaced by
/// another branch.
///
/// Text form (used in configuration, CLI and API):
///
/// | Text | Meaning |
/// |---|---|
/// | `branch:development` | local branch `refs/heads/development`, including unpushed commits |
/// | `remote:origin/development` | remote-tracking branch, after an authorised fetch |
/// | `tag:v2.1.0` | a tag (fixed, reproducible) |
/// | `commit:<40 or 64 hex>` | a full commit id (fixed, reproducible) |
/// | `worktree` | whatever the checked-out worktree's `HEAD` points to |
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum TrackTarget {
    /// A local branch.
    Branch(String),
    /// A remote-tracking branch.
    Remote {
        /// Remote name, e.g. `origin`.
        remote: String,
        /// Branch name on the remote.
        branch: String,
    },
    /// A tag.
    Tag(String),
    /// A full commit id in lowercase hex (SHA-1 or SHA-256).
    Commit(String),
    /// The worktree's own `HEAD`.
    WorktreeHead,
}

/// Error returned when text is not a valid [`TrackTarget`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrackTargetError {
    /// The text has no recognised `kind:` prefix.
    #[error(
        "track target `{0}` must be one of `branch:<name>`, `remote:<remote>/<branch>`, `tag:<name>`, `commit:<full id>` or `worktree`"
    )]
    UnknownKind(String),
    /// The ref name violates git's ref-name rules.
    #[error("`{0}` is not a valid git ref name")]
    InvalidRefName(String),
    /// A remote target lacks the `<remote>/` part.
    #[error("remote target `{0}` must look like `<remote>/<branch>`")]
    MissingRemote(String),
    /// A commit target is not a full lowercase hex id.
    #[error("commit `{0}` must be a full 40- or 64-digit lowercase hex id")]
    InvalidCommit(String),
}

impl TrackTarget {
    /// Whether the target can never move (tag or commit). Tags can be
    /// re-pointed in git, but Knowell treats a moved tag as an event to report.
    pub fn is_fixed(&self) -> bool {
        matches!(self, TrackTarget::Tag(_) | TrackTarget::Commit(_))
    }

    /// The fully qualified git ref this target resolves through, if any.
    pub fn full_ref(&self) -> Option<String> {
        match self {
            TrackTarget::Branch(name) => Some(format!("refs/heads/{name}")),
            TrackTarget::Remote { remote, branch } => {
                Some(format!("refs/remotes/{remote}/{branch}"))
            }
            TrackTarget::Tag(name) => Some(format!("refs/tags/{name}")),
            TrackTarget::Commit(_) | TrackTarget::WorktreeHead => None,
        }
    }
}

impl fmt::Display for TrackTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackTarget::Branch(name) => write!(f, "branch:{name}"),
            TrackTarget::Remote { remote, branch } => write!(f, "remote:{remote}/{branch}"),
            TrackTarget::Tag(name) => write!(f, "tag:{name}"),
            TrackTarget::Commit(id) => write!(f, "commit:{id}"),
            TrackTarget::WorktreeHead => f.write_str("worktree"),
        }
    }
}

impl fmt::Debug for TrackTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackTarget({self})")
    }
}

impl FromStr for TrackTarget {
    type Err = TrackTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "worktree" {
            return Ok(TrackTarget::WorktreeHead);
        }
        let Some((kind, value)) = s.split_once(':') else {
            return Err(TrackTargetError::UnknownKind(s.to_owned()));
        };
        match kind {
            "branch" => Ok(TrackTarget::Branch(valid_ref_name(value)?)),
            "tag" => Ok(TrackTarget::Tag(valid_ref_name(value)?)),
            "remote" => {
                let Some((remote, branch)) = value.split_once('/') else {
                    return Err(TrackTargetError::MissingRemote(value.to_owned()));
                };
                Ok(TrackTarget::Remote {
                    remote: valid_ref_name(remote)?,
                    branch: valid_ref_name(branch)?,
                })
            }
            "commit" => {
                let full_length = value.len() == 40 || value.len() == 64;
                let hex = value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if full_length && hex {
                    Ok(TrackTarget::Commit(value.to_owned()))
                } else {
                    Err(TrackTargetError::InvalidCommit(value.to_owned()))
                }
            }
            _ => Err(TrackTargetError::UnknownKind(s.to_owned())),
        }
    }
}

/// Checks the subset of `git check-ref-format` rules that matter for
/// user-supplied branch, tag and remote names.
fn valid_ref_name(name: &str) -> Result<String, TrackTargetError> {
    let invalid = || TrackTargetError::InvalidRefName(name.to_owned());
    if name.is_empty() || name == "@" || name.ends_with('/') || name.ends_with('.') {
        return Err(invalid());
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return Err(invalid());
    }
    let forbidden = |c: char| c.is_ascii_control() || " ~^:?*[\\".contains(c);
    if name.chars().any(forbidden) {
        return Err(invalid());
    }
    if name
        .split('/')
        .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(invalid());
    }
    Ok(name.to_owned())
}

impl Serialize for TrackTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TrackTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for TrackTarget {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TrackTarget".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        crate::schema::string_schema(
            "Ref to follow: branch:<name>, remote:<remote>/<branch>, tag:<name>, commit:<sha> or worktree.",
            Some(
                "^(worktree|branch:.+|remote:[^/]+/.+|tag:.+|commit:([0-9a-f]{40}|[0-9a-f]{64}))$",
            ),
            &["branch:development"],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        let sha1 = "a".repeat(40);
        let sha256 = "0123456789abcdef".repeat(4);
        let cases = [
            "branch:development".to_owned(),
            "branch:feature/payment-retry".to_owned(),
            "remote:origin/release/2.x".to_owned(),
            "tag:v2.1.0".to_owned(),
            format!("commit:{sha1}"),
            format!("commit:{sha256}"),
            "worktree".to_owned(),
        ];
        for text in cases {
            let target: TrackTarget = text.parse().unwrap();
            assert_eq!(target.to_string(), text);
        }
    }

    #[test]
    fn remote_splits_on_first_slash() {
        let t: TrackTarget = "remote:origin/release/2.x".parse().unwrap();
        assert_eq!(
            t,
            TrackTarget::Remote {
                remote: "origin".into(),
                branch: "release/2.x".into()
            }
        );
        assert_eq!(t.full_ref().unwrap(), "refs/remotes/origin/release/2.x");
    }

    #[test]
    fn rejects_guessable_or_invalid_targets() {
        assert!(matches!(
            "development".parse::<TrackTarget>(),
            Err(TrackTargetError::UnknownKind(_))
        ));
        assert!(matches!(
            "remote:origin".parse::<TrackTarget>(),
            Err(TrackTargetError::MissingRemote(_))
        ));
        assert!(matches!(
            "commit:abc1234".parse::<TrackTarget>(),
            Err(TrackTargetError::InvalidCommit(_))
        ));
        for bad in [
            "branch:",
            "branch:a..b",
            "branch:a b",
            "branch:x.lock",
            "branch:.hidden",
            "branch:a/",
            "branch:a@{1}",
            "tag:v1^",
        ] {
            assert!(
                matches!(
                    bad.parse::<TrackTarget>(),
                    Err(TrackTargetError::InvalidRefName(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn fixedness() {
        assert!("tag:v1".parse::<TrackTarget>().unwrap().is_fixed());
        assert!(!"branch:main".parse::<TrackTarget>().unwrap().is_fixed());
        assert!(!TrackTarget::WorktreeHead.is_fixed());
    }

    #[test]
    fn serde_uses_text_form() {
        let t: TrackTarget = serde_json::from_str("\"branch:development\"").unwrap();
        assert_eq!(t, TrackTarget::Branch("development".into()));
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"branch:development\"");
    }
}
