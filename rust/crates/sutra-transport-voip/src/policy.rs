//! Toll-fraud controls on `voip://originate`: which destinations a flow may dial, and how often.
//!
//! A flow that can place calls can be made to place expensive ones. So an originate passes four
//! checks, in order, and is refused (with a diagnostic and an audit log event) at the first one it
//! fails:
//!
//! 1. **The allow-list** (`SUTRA_VOIP_ORIGINATE_ALLOW`): glob patterns (`*` any run, `?` one
//!    character, case-insensitive) matched against the destination's number (`+15551230000`,
//!    from a bare number, a `tel:` URI or a numeric SIP user part) and against its SIP URI
//!    (`sip:1001@pbx.internal`). **The default is deny-all**: until the list is configured, no
//!    destination may be dialled. `*` allows every destination, deliberately.
//! 2. **Premium-rate prefixes** (`SUTRA_VOIP_ORIGINATE_BLOCK_PREFIXES`, a built-in list by
//!    default) are refused, unless `SUTRA_VOIP_ORIGINATE_ALLOW_PREMIUM=true` or the destination
//!    is matched by an allow pattern that names it specifically (a literal prefix of four
//!    characters or more before any wildcard).
//! 3. **International numbers** — `+`, `00` or `011` numbers outside
//!    `SUTRA_VOIP_ORIGINATE_HOME_PREFIX` (`+1`, say; with none set, every international-format
//!    number) — are refused, unless `SUTRA_VOIP_ORIGINATE_ALLOW_INTERNATIONAL=true` or a
//!    specific allow pattern names them, as above.
//! 4. **Rate limits**: per destination (`SUTRA_VOIP_ORIGINATE_RATE_PER_DESTINATION`, calls per
//!    hour, default 10) and overall (`SUTRA_VOIP_ORIGINATE_RATE_GLOBAL`, calls per minute,
//!    default 60). A rate refusal is retryable; the others are permanent.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Premium-rate prefixes refused by default (E.164): North American 900 / 976, and the
/// premium and special-rate ranges of several European and Pacific numbering plans.
pub const DEFAULT_BLOCKED_PREFIXES: [&str; 16] = [
    "+1900", "+1976", "+4487", "+449", "+3381", "+3382", "+3389", "+49900", "+39899", "+34803",
    "+34806", "+34807", "+43900", "+41900", "+61190", "+64900",
];
/// Originates per destination per hour, by default.
pub const DEFAULT_RATE_PER_DESTINATION_PER_HOUR: u32 = 10;
/// Originates in total per minute, by default.
pub const DEFAULT_RATE_GLOBAL_PER_MINUTE: u32 = 60;

/// The configured controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginatePolicy {
    /// Allowed destination patterns; empty = nothing may be dialled.
    pub allow: Vec<String>,
    pub allow_international: bool,
    pub allow_premium: bool,
    /// The home numbering plan's prefix (`+1`): numbers under it are not international.
    pub home_prefix: Option<String>,
    pub blocked_prefixes: Vec<String>,
    /// 0 = unlimited.
    pub per_destination_per_hour: u32,
    /// 0 = unlimited.
    pub global_per_minute: u32,
}

impl Default for OriginatePolicy {
    fn default() -> Self {
        OriginatePolicy {
            allow: Vec::new(),
            allow_international: false,
            allow_premium: false,
            home_prefix: None,
            blocked_prefixes: DEFAULT_BLOCKED_PREFIXES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            per_destination_per_hour: DEFAULT_RATE_PER_DESTINATION_PER_HOUR,
            global_per_minute: DEFAULT_RATE_GLOBAL_PER_MINUTE,
        }
    }
}

impl OriginatePolicy {
    /// The defaults with `patterns` allowed (tests and examples).
    pub fn allowing(patterns: &[&str]) -> OriginatePolicy {
        OriginatePolicy {
            allow: patterns.iter().map(|p| p.to_string()).collect(),
            ..OriginatePolicy::default()
        }
    }
}

/// Which rule refused an originate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No allow-list is configured.
    NotConfigured,
    NotAllowed,
    Premium(String),
    International,
    RatePerDestination,
    RateGlobal,
}

impl Refusal {
    /// The rule's name, as audit events and diagnostics carry it.
    pub fn rule(&self) -> &'static str {
        match self {
            Refusal::NotConfigured => "allow-list-not-configured",
            Refusal::NotAllowed => "not-on-allow-list",
            Refusal::Premium(_) => "premium-prefix",
            Refusal::International => "international",
            Refusal::RatePerDestination => "rate-per-destination",
            Refusal::RateGlobal => "rate-global",
        }
    }

    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Refusal::RatePerDestination | Refusal::RateGlobal)
    }

    pub fn explain(&self) -> String {
        match self {
            Refusal::NotConfigured => {
                "originate is disabled until SUTRA_VOIP_ORIGINATE_ALLOW lists the destinations a flow may dial".to_string()
            }
            Refusal::NotAllowed => {
                "the destination is not on the originate allow-list (SUTRA_VOIP_ORIGINATE_ALLOW)".to_string()
            }
            Refusal::Premium(prefix) => format!(
                "the destination is a premium-rate number ({prefix}…); allow it specifically or set SUTRA_VOIP_ORIGINATE_ALLOW_PREMIUM=true"
            ),
            Refusal::International => {
                "the destination is international; allow it specifically or set SUTRA_VOIP_ORIGINATE_ALLOW_INTERNATIONAL=true (SUTRA_VOIP_ORIGINATE_HOME_PREFIX names the home numbering plan)".to_string()
            }
            Refusal::RatePerDestination => {
                "too many calls to this destination in the last hour (SUTRA_VOIP_ORIGINATE_RATE_PER_DESTINATION)".to_string()
            }
            Refusal::RateGlobal => {
                "too many calls in the last minute (SUTRA_VOIP_ORIGINATE_RATE_GLOBAL)".to_string()
            }
        }
    }
}

/// The policy and its rate windows.
pub struct OriginateGuard {
    policy: OriginatePolicy,
    windows: Mutex<Windows>,
}

#[derive(Default)]
struct Windows {
    global: VecDeque<Instant>,
    per_destination: HashMap<String, VecDeque<Instant>>,
}

/// Whether `pattern` (glob: `*`, `?`) matches `text`, ignoring case.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The literal text of `pattern` before its first wildcard.
fn literal_prefix(pattern: &str) -> &str {
    let end = pattern.find(['*', '?']).unwrap_or(pattern.len());
    &pattern[..end]
}

/// The destination's number, when it has one: digits with a leading `+` for an international
/// format (`00…` and `011…` are rewritten to `+…`). `None` for a SIP user that is not a number.
pub fn destination_number(destination: &str) -> Option<String> {
    let user = crate::hub::party_number(destination);
    let compact: String = user
        .chars()
        .filter(|c| !matches!(c, '-' | '.' | ' ' | '(' | ')'))
        .collect();
    let digits = compact.strip_prefix('+').unwrap_or(&compact);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if compact.starts_with('+') {
        return Some(compact);
    }
    if let Some(rest) = compact.strip_prefix("011") {
        if rest.len() >= 7 {
            return Some(format!("+{rest}"));
        }
    }
    if let Some(rest) = compact.strip_prefix("00") {
        if rest.len() >= 7 {
            return Some(format!("+{rest}"));
        }
    }
    Some(compact)
}

impl OriginateGuard {
    pub fn new(policy: OriginatePolicy) -> OriginateGuard {
        OriginateGuard {
            policy,
            windows: Mutex::new(Windows::default()),
        }
    }

    pub fn policy(&self) -> &OriginatePolicy {
        &self.policy
    }

    /// Check `raw` (the destination as the flow gave it) and `sip_uri` (what will be dialled);
    /// a destination that passes is counted against the rate limits.
    pub fn check(&self, raw: &str, sip_uri: &str) -> Result<(), Refusal> {
        self.check_at(raw, sip_uri, Instant::now())
    }

    pub fn check_at(&self, raw: &str, sip_uri: &str, now: Instant) -> Result<(), Refusal> {
        let policy = &self.policy;
        if policy.allow.is_empty() {
            return Err(Refusal::NotConfigured);
        }
        let number = destination_number(raw).or_else(|| destination_number(sip_uri));
        let mut candidates: Vec<String> = vec![raw.trim().to_string(), sip_uri.to_string()];
        if let Some(n) = &number {
            candidates.push(n.clone());
        }
        let matching: Vec<&String> = policy
            .allow
            .iter()
            .filter(|p| candidates.iter().any(|c| glob_matches(p, c)))
            .collect();
        if matching.is_empty() {
            return Err(Refusal::NotAllowed);
        }
        // A pattern names a destination specifically when its literal prefix (4+ characters)
        // is a prefix of the number.
        let specific = |number: &str| {
            matching.iter().any(|p| {
                let lit = literal_prefix(p);
                lit.len() >= 4 && number.to_lowercase().starts_with(&lit.to_lowercase())
            })
        };
        if let Some(number) = &number {
            let national_premium = |n: &str| {
                (n.len() == 11 && (n.starts_with("1900") || n.starts_with("1976")))
                    || (n.len() == 10 && (n.starts_with("900") || n.starts_with("976")))
            };
            let premium = policy
                .blocked_prefixes
                .iter()
                .find(|prefix| number.starts_with(prefix.as_str()))
                .cloned()
                .or_else(|| national_premium(number).then(|| number[..4].to_string()));
            if let Some(prefix) = premium {
                if !policy.allow_premium && !specific(number) {
                    return Err(Refusal::Premium(prefix));
                }
            }
            let international = number.starts_with('+')
                && policy
                    .home_prefix
                    .as_deref()
                    .is_none_or(|home| !number.starts_with(home));
            if international && !policy.allow_international && !specific(number) {
                return Err(Refusal::International);
            }
        }
        let key = number.unwrap_or_else(|| sip_uri.to_ascii_lowercase());
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let hour = Duration::from_secs(3600);
        let minute = Duration::from_secs(60);
        windows
            .per_destination
            .retain(|_, times| times.back().is_some_and(|t| now.duration_since(*t) < hour));
        if policy.per_destination_per_hour > 0 {
            let times = windows.per_destination.entry(key.clone()).or_default();
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t) >= hour)
            {
                times.pop_front();
            }
            if times.len() as u32 >= policy.per_destination_per_hour {
                return Err(Refusal::RatePerDestination);
            }
        }
        while windows
            .global
            .front()
            .is_some_and(|t| now.duration_since(*t) >= minute)
        {
            windows.global.pop_front();
        }
        if policy.global_per_minute > 0 && windows.global.len() as u32 >= policy.global_per_minute {
            return Err(Refusal::RateGlobal);
        }
        windows.global.push_back(now);
        windows
            .per_destination
            .entry(key)
            .or_default()
            .push_back(now);
        Ok(())
    }
}

/// A destination as a log line may carry it: the first characters and the last two.
pub fn masked(destination: &str) -> String {
    let chars: Vec<char> = destination.chars().collect();
    if chars.len() <= 6 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(policy: OriginatePolicy) -> OriginateGuard {
        OriginateGuard::new(policy)
    }

    #[test]
    fn nothing_is_dialled_until_an_allow_list_is_configured() {
        let g = guard(OriginatePolicy::default());
        assert_eq!(
            g.check("+15551230000", "sip:+15551230000@trunk"),
            Err(Refusal::NotConfigured)
        );
    }

    #[test]
    fn the_allow_list_matches_numbers_and_sip_uris() {
        let g = guard(OriginatePolicy {
            home_prefix: Some("+1".into()),
            ..OriginatePolicy::allowing(&["+1555*", "sip:*@pbx.internal"])
        });
        assert_eq!(g.check("+15551230000", "sip:+15551230000@trunk"), Ok(()));
        assert_eq!(
            g.check("tel:+1-555-123-0001", "sip:+15551230001@trunk"),
            Ok(())
        );
        assert_eq!(
            g.check("sip:1001@pbx.internal", "sip:1001@pbx.internal"),
            Ok(())
        );
        assert_eq!(
            g.check("+16665550000", "sip:+16665550000@trunk"),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(
            g.check("sip:1001@elsewhere", "sip:1001@elsewhere"),
            Err(Refusal::NotAllowed)
        );
    }

    #[test]
    fn premium_and_international_numbers_need_a_specific_allowance() {
        let g = guard(OriginatePolicy {
            home_prefix: Some("+1".into()),
            ..OriginatePolicy::allowing(&["*"])
        });
        assert!(matches!(
            g.check("+19005550100", "sip:+19005550100@t"),
            Err(Refusal::Premium(_))
        ));
        assert!(matches!(
            g.check("19005550100", "sip:19005550100@t"),
            Err(Refusal::Premium(_))
        ));
        assert_eq!(
            g.check("+442071234567", "sip:+442071234567@t"),
            Err(Refusal::International)
        );
        assert_eq!(
            g.check("011442071234567", "sip:011442071234567@t"),
            Err(Refusal::International)
        );
        assert_eq!(g.check("+15551230000", "sip:+15551230000@t"), Ok(()));
        // Named specifically: allowed.
        let g = guard(OriginatePolicy {
            home_prefix: Some("+1".into()),
            ..OriginatePolicy::allowing(&["+4420*", "+19005550100"])
        });
        assert_eq!(g.check("+442071234567", "sip:+442071234567@t"), Ok(()));
        assert_eq!(g.check("+19005550100", "sip:+19005550100@t"), Ok(()));
        // Or by the switches.
        let g = guard(OriginatePolicy {
            allow_international: true,
            allow_premium: true,
            ..OriginatePolicy::allowing(&["*"])
        });
        assert_eq!(g.check("+442071234567", "sip:+442071234567@t"), Ok(()));
        assert_eq!(g.check("+19005550100", "sip:+19005550100@t"), Ok(()));
        // With no home prefix, every +number is international.
        let g = guard(OriginatePolicy::allowing(&["+1*"]));
        assert_eq!(
            g.check("+15551230000", "sip:+15551230000@t"),
            Err(Refusal::International)
        );
        // An extension is neither.
        let g = guard(OriginatePolicy::allowing(&["sip:*@pbx"]));
        assert_eq!(g.check("sip:2001@pbx", "sip:2001@pbx"), Ok(()));
    }

    #[test]
    fn rate_limits_hold_per_destination_and_overall() {
        let g = guard(OriginatePolicy {
            per_destination_per_hour: 2,
            global_per_minute: 3,
            home_prefix: Some("+1".into()),
            ..OriginatePolicy::allowing(&["+1555*"])
        });
        let t0 = Instant::now();
        let a = ("+15550000001", "sip:+15550000001@t");
        assert_eq!(g.check_at(a.0, a.1, t0), Ok(()));
        assert_eq!(g.check_at(a.0, a.1, t0), Ok(()));
        assert_eq!(g.check_at(a.0, a.1, t0), Err(Refusal::RatePerDestination));
        assert_eq!(g.check_at("+15550000002", "sip:+15550000002@t", t0), Ok(()));
        assert_eq!(
            g.check_at("+15550000003", "sip:+15550000003@t", t0),
            Err(Refusal::RateGlobal)
        );
        // A minute later the global window has room; the hour still holds for `a`.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(
            g.check_at("+15550000003", "sip:+15550000003@t", later),
            Ok(())
        );
        assert_eq!(
            g.check_at(a.0, a.1, later),
            Err(Refusal::RatePerDestination)
        );
        assert_eq!(g.check_at(a.0, a.1, t0 + Duration::from_secs(3601)), Ok(()));
    }

    #[test]
    fn globs_and_masks() {
        assert!(glob_matches("+1555*", "+15551230000"));
        assert!(glob_matches("sip:?00?@PBX", "sip:1001@pbx"));
        assert!(!glob_matches("+1555", "+15551"));
        assert!(glob_matches("*", ""));
        assert_eq!(
            destination_number("sip:+1 (555) 123@trunk").as_deref(),
            Some("+1555123")
        );
        assert_eq!(destination_number("sip:alice@pbx"), None);
        assert_eq!(masked("+15551230000"), "+155…00");
    }
}
