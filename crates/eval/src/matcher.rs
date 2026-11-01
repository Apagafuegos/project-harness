//! Deterministic, dependency-free requirement matchers.
//!
//! Scoring never asks a model whether something was retained. A requirement is
//! retained when its declared terms occur in the scored text. Both the text and
//! the terms are compared after [`normalize`], which lowercases and collapses
//! every run of whitespace to one space, so a phrase that a projection wrapped
//! across lines or indented still matches. Nothing else is rewritten: matching
//! is a plain substring test, never a regular expression, so a term that looks
//! like a pattern is matched literally. See ADR 0027.

/// One requirement stated in a task's history, with the terms that prove it
/// survived compaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// Stable identifier, unique within the task.
    pub id: String,
    /// The exact phrasing stated in the history, for readers of the task.
    pub text: String,
    /// How the requirement is scored.
    pub matcher: Matcher,
}

/// An any-of/all-of conjunction of normalized substrings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matcher {
    terms: Vec<String>,
    any_of: Vec<String>,
}

impl Matcher {
    /// Build a matcher from raw terms. Every term must normalize to a nonempty
    /// string, and at least one term (in either list) is required.
    pub fn new(
        all_of: impl IntoIterator<Item = String>,
        any_of: impl IntoIterator<Item = String>,
    ) -> Result<Self, String> {
        let terms = normalized_terms(all_of)?;
        let any_of = normalized_terms(any_of)?;
        if terms.is_empty() && any_of.is_empty() {
            return Err("matcher needs at least one term".into());
        }
        Ok(Self { terms, any_of })
    }

    /// Whether every required term and at least one alternative (when any are
    /// declared) occur in `normalized`, a value already passed through
    /// [`normalize`].
    pub fn matches(&self, normalized: &str) -> bool {
        self.terms.iter().all(|term| normalized.contains(term))
            && (self.any_of.is_empty() || self.any_of.iter().any(|term| normalized.contains(term)))
    }
}

fn normalized_terms(terms: impl IntoIterator<Item = String>) -> Result<Vec<String>, String> {
    terms
        .into_iter()
        .map(|term| {
            let normalized = normalize(&term);
            if normalized.is_empty() {
                return Err("matcher term is empty after normalization".to_owned());
            }
            Ok(normalized)
        })
        .collect()
}

/// Lowercase and collapse every run of whitespace to a single space, trimming
/// the ends. This is the only normalization scoring applies.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.extend(character.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(all: &[&str], any: &[&str]) -> Requirement {
        Requirement {
            id: "R1".into(),
            text: "stated".into(),
            matcher: Matcher::new(
                all.iter().map(|s| s.to_string()),
                any.iter().map(|s| s.to_string()),
            )
            .unwrap(),
        }
    }

    #[test]
    fn normalization_collapses_whitespace_and_case_only() {
        assert_eq!(
            normalize("  Keep  the\n\tPUBLIC name  "),
            "keep the public name"
        );
        // Punctuation is preserved: a matcher is not a tokenizer.
        assert_eq!(normalize("flag: --json"), "flag: --json");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn all_of_requires_every_term_and_any_of_one_alternative() {
        let r = requirement(
            &["public name", "report_row"],
            &["byte-identical", "same output"],
        );
        assert!(r.matcher.matches(&normalize(
            "Keep report_row's PUBLIC name; output stays byte-identical."
        )));
        assert!(!r.matcher.matches(&normalize(
            "Keep the PUBLIC name; output stays byte-identical."
        )));
        assert!(!r.matcher.matches(&normalize(
            "Keep report_row's public name; output is unchanged."
        )));
        let any = requirement(&[], &["byte-identical", "same output"]);
        assert!(!any.matcher.matches(&normalize("nothing relevant")));
        assert!(any.matcher.matches(&normalize("the same   output")));
    }

    #[test]
    fn an_empty_or_whitespace_term_is_rejected() {
        assert!(Matcher::new(Vec::new(), Vec::new()).is_err());
        assert!(Matcher::new(["   ".to_owned()], Vec::new()).is_err());
        assert!(Matcher::new(Vec::new(), ["\n".to_owned()]).is_err());
    }
}
