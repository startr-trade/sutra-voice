//! Number binding: which `call-start` channel answers a called number.
//!
//! A `call-start` channel's `bind:` names its numbers: `number:+15551230000`, a
//! comma-separated list (`number:+15551230000,+15551230001`, each item may repeat the
//! `number:` prefix), a prefix `number:+1555123*`, or the catch-all `number:*`. The most
//! specific match wins: exact > longest prefix > catch-all. Two definitions binding the same
//! exact number, the same prefix, or both the catch-all fail closed with
//! [`crate::codes::BIND_DUPLICATE`].
//!
//! Numbers are compared on their digits only, so `+1 (555) 123-0000`, `+15551230000`,
//! `15551230000` and `sip:+15551230000@carrier.example` all name the same number.

use std::collections::BTreeMap;

use sutra_channels::Diagnostic;

use crate::codes;

/// The prefix every number bind starts with.
pub const NUMBER_BIND_PREFIX: &str = "number:";

/// One parsed pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NumberPattern {
    Exact(String),
    Prefix(String),
    Any,
}

impl NumberPattern {
    fn describe(&self) -> String {
        match self {
            NumberPattern::Exact(n) => format!("number:{n}"),
            NumberPattern::Prefix(p) => format!("number:{p}*"),
            NumberPattern::Any => "number:*".to_string(),
        }
    }
}

/// Reduce a number, `tel:` URI or SIP URI to its digits. A SIP/tel URI contributes its user
/// part only (before `@`, without `;` parameters).
pub fn normalize_number(raw: &str) -> String {
    let mut value = raw.trim();
    for scheme in ["sips:", "sip:", "tel:"] {
        if value.len() >= scheme.len() && value[..scheme.len()].eq_ignore_ascii_case(scheme) {
            value = &value[scheme.len()..];
            break;
        }
    }
    let value = value.split('@').next().unwrap_or("");
    let value = value.split(';').next().unwrap_or("");
    value.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Whether `spec` is a number bind at all.
pub fn is_number_bind(spec: &str) -> bool {
    spec.trim()
        .to_ascii_lowercase()
        .starts_with(NUMBER_BIND_PREFIX)
}

/// Parse a `bind:` value into its patterns.
pub fn parse_number_bind(spec: &str) -> Result<Vec<NumberPattern>, String> {
    let trimmed = spec.trim();
    if !is_number_bind(trimmed) {
        return Err(format!(
            "'{spec}' is not a number bind (expected 'number:…')"
        ));
    }
    let mut patterns = Vec::new();
    for item in trimmed[NUMBER_BIND_PREFIX.len()..].split(',') {
        let mut item = item.trim();
        if item.len() >= NUMBER_BIND_PREFIX.len()
            && item[..NUMBER_BIND_PREFIX.len()].eq_ignore_ascii_case(NUMBER_BIND_PREFIX)
        {
            item = item[NUMBER_BIND_PREFIX.len()..].trim();
        }
        if item.is_empty() {
            return Err(format!("'{spec}' has an empty number"));
        }
        let pattern = if item == "*" {
            NumberPattern::Any
        } else if let Some(prefix) = item.strip_suffix('*') {
            let digits = normalize_number(prefix);
            if digits.is_empty() {
                return Err(format!("'{item}' has no digits before '*'"));
            }
            NumberPattern::Prefix(digits)
        } else {
            if item.contains('*') {
                return Err(format!("'{item}': '*' is only allowed at the end"));
            }
            let digits = normalize_number(item);
            if digits.is_empty() {
                return Err(format!("'{item}' has no digits"));
            }
            NumberPattern::Exact(digits)
        };
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    Ok(patterns)
}

/// A resolved routing table from patterns to targets.
#[derive(Debug, Clone)]
pub struct NumberRouter<T> {
    exact: BTreeMap<String, T>,
    /// Sorted longest first.
    prefixes: Vec<(String, T)>,
    any: Option<T>,
}

impl<T> Default for NumberRouter<T> {
    fn default() -> Self {
        NumberRouter {
            exact: BTreeMap::new(),
            prefixes: Vec::new(),
            any: None,
        }
    }
}

impl<T: Clone> NumberRouter<T> {
    /// Build from `(owner, bind spec, target)` entries; `owner` names the definition in
    /// diagnostics. Fails closed on an invalid spec or a duplicate pattern.
    pub fn build<'a>(
        entries: impl IntoIterator<Item = (String, &'a str, T)>,
    ) -> Result<NumberRouter<T>, Diagnostic> {
        let mut router = NumberRouter::default();
        let mut owners: BTreeMap<NumberPattern, String> = BTreeMap::new();
        for (owner, spec, target) in entries {
            let patterns = parse_number_bind(spec).map_err(|reason| {
                Diagnostic::error(codes::BIND_INVALID, format!("{owner}: {reason}"))
                    .with_attribute("channel", owner.clone())
            })?;
            for pattern in patterns {
                if let Some(previous) = owners.get(&pattern) {
                    return Err(Diagnostic::error(
                        codes::BIND_DUPLICATE,
                        format!(
                            "{} is bound by both '{previous}' and '{owner}'; a number may be bound by one channel only",
                            pattern.describe()
                        ),
                    )
                    .with_attribute("channel", owner.clone())
                    .with_attribute("conflictsWith", previous.clone()));
                }
                owners.insert(pattern.clone(), owner.clone());
                match pattern {
                    NumberPattern::Exact(n) => {
                        router.exact.insert(n, target.clone());
                    }
                    NumberPattern::Prefix(p) => router.prefixes.push((p, target.clone())),
                    NumberPattern::Any => router.any = Some(target.clone()),
                }
            }
        }
        router
            .prefixes
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(router)
    }

    /// The most specific target for `called`, if any.
    pub fn resolve(&self, called: &str) -> Option<&T> {
        let digits = normalize_number(called);
        if !digits.is_empty() {
            if let Some(target) = self.exact.get(&digits) {
                return Some(target);
            }
            if let Some((_, target)) = self.prefixes.iter().find(|(p, _)| digits.starts_with(p)) {
                return Some(target);
            }
        }
        self.any.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefixes.is_empty() && self.any.is_none()
    }
}
