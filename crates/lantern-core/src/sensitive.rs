//! Local detection of files that must never reach the agent: keys, tokens and credential stores.
//! A false positive only keeps a file out of the index, so the checks lean towards flagging.

use std::sync::LazyLock;

use regex::RegexSet;

/// File names that hold credentials whatever their content.
const NAMES: &[(&str, &str)] = &[
    (r"^\.env(\..+)?$", "an environment file"),
    (r"^id_(rsa|dsa|ecdsa|ed25519)(_sk)?$", "an SSH private key"),
    (
        r"(?i)\.(pem|key|p12|pfx|jks|keystore|ppk|asc|gpg)$",
        "a key file",
    ),
    (
        r"(?i)\.(kdbx?|keychain(-db)?|1pif|agilekeychain)$",
        "a password store",
    ),
    (r"(?i)^(credentials|secrets?)(\..+)?$", "a credentials file"),
    (
        r"^\.?(netrc|pgpass|npmrc|pypirc|git-credentials)$",
        "a credentials file",
    ),
];

/// Content patterns for secrets that are recognizable on sight.
const CONTENT: &[(&str, &str)] = &[
    (r"-----BEGIN [A-Z ]*PRIVATE KEY", "a private key"),
    (r"\bAKIA[0-9A-Z]{16}\b", "an AWS access key"),
    (
        r"\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})",
        "a GitHub token",
    ),
    (r"\bsk-(ant-|proj-)?[A-Za-z0-9_-]{20,}", "an API key"),
    (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", "a Slack token"),
    (r"\bAIza[0-9A-Za-z_-]{35}\b", "a Google API key"),
    (r"\b[rs]k_live_[0-9A-Za-z]{20,}", "a Stripe key"),
    (
        r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.",
        "a login token",
    ),
];

static NAME_SET: LazyLock<RegexSet> = LazyLock::new(|| set(NAMES));
static CONTENT_SET: LazyLock<RegexSet> = LazyLock::new(|| set(CONTENT));

fn set(patterns: &[(&str, &str)]) -> RegexSet {
    RegexSet::new(patterns.iter().map(|(p, _)| p)).expect("sensitive patterns are valid")
}

/// Why a file with this name is sensitive, if it is.
pub fn by_name(name: &str) -> Option<&'static str> {
    NAME_SET.matches(name).iter().next().map(|i| NAMES[i].1)
}

/// Why this text is sensitive, if it is.
pub fn in_text(text: &str) -> Option<&'static str> {
    CONTENT_SET
        .matches(text)
        .iter()
        .next()
        .map(|i| CONTENT[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for name in [
            ".env",
            ".env.local",
            "id_ed25519",
            "server.PEM",
            "vault.kdbx",
            ".netrc",
        ] {
            assert!(by_name(name).is_some(), "{name}");
        }
        for name in [
            "environment.md",
            "id_card.jpg",
            "keys.txt",
            "notes.txt",
            "id_rsa.pub",
        ] {
            assert!(by_name(name).is_none(), "{name}");
        }
    }

    #[test]
    fn content() {
        assert!(in_text("-----BEGIN OPENSSH PRIVATE KEY-----\nabc").is_some());
        assert!(in_text("aws_access_key_id = AKIAIOSFODNN7EXAMPLE").is_some());
        assert!(in_text("token: ghp_abcdefghijklmnopqrstuvwxyz0123456789").is_some());
        assert!(in_text("ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuv").is_some());
        assert!(in_text("Shopping list: eggs, milk, a ski mask").is_none());
        assert!(in_text("-----BEGIN PUBLIC KEY-----").is_none());
    }
}
