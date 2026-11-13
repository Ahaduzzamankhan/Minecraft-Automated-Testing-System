//! Minecraft-aware version ordering and constraint matching.
//!
//! Minecraft moved to calendar versions in 2026 (`26.1`, `26.2`, ...). Plain
//! lexicographic or semver comparison cannot tell that `26.2` is newer than
//! `1.21.11`, so MATS parses both eras into numeric components and compares
//! them component-wise.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A Minecraft version string, ordered across the legacy and calendar eras.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct McVersion(String);

impl McVersion {
    pub fn new(raw: impl Into<String>) -> Self {
        McVersion(raw.into().trim().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parsed(&self) -> ComparableVersion {
        ComparableVersion::parse(&self.0)
    }

    /// Release year for calendar versions, `None` for legacy versions.
    pub fn calendar_year(&self) -> Option<u32> {
        let parsed = self.parsed();
        if parsed.calendar {
            parsed.parts.first().copied().map(|year| year as u32)
        } else {
            None
        }
    }

    pub fn is_calendar(&self) -> bool {
        self.parsed().calendar
    }

    /// `true` for `25w14a`-style snapshots.
    pub fn is_snapshot(&self) -> bool {
        let raw = self.0.as_str();
        let mut chars = raw.chars();
        let week = raw.len() >= 5
            && chars.next().is_some_and(|c| c.is_ascii_digit())
            && raw.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
            && raw.chars().nth(2) == Some('w');
        week
    }
}

impl fmt::Display for McVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for McVersion {
    fn from(value: &str) -> Self {
        McVersion::new(value)
    }
}

impl From<String> for McVersion {
    fn from(value: String) -> Self {
        McVersion::new(value)
    }
}

impl PartialOrd for McVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for McVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parsed().cmp(&other.parsed())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComparableVersion {
    calendar: bool,
    parts: Vec<u64>,
    /// Pre-release suffix such as `rc-3`, `pre-2`, `snapshot-1`; `None` is a
    /// final release and sorts above any suffix.
    suffix: Option<String>,
    /// Raw text for versions that could not be parsed numerically.
    unparsed: Option<String>,
}

impl ComparableVersion {
    fn parse(raw: &str) -> Self {
        let text = raw.trim();
        if text.is_empty() {
            return ComparableVersion::unparsed(text);
        }
        // Strip a leading `v` used by some launchers.
        let text = text.strip_prefix('v').unwrap_or(text);

        // Legacy snapshots (`25w14a`) have no numeric dot prefix.
        if text.len() >= 3
            && text.as_bytes()[0].is_ascii_digit()
            && text.as_bytes()[1].is_ascii_digit()
            && text.as_bytes()[2] == b'w'
        {
            // Treat snapshots as belonging to their week's year, below the
            // matching release. They are only used for display and never
            // picked automatically.
            let year = text[..2].parse::<u64>().unwrap_or(0) + 2000;
            let rest: String = text[3..].to_string();
            let week: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let week_num = week.parse::<u64>().unwrap_or(0);
            return ComparableVersion {
                calendar: false,
                parts: vec![year, week_num],
                suffix: Some(format!("snapshot-{raw}")),
                unparsed: None,
            };
        }

        let (numeric, suffix) = match text.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
            Some(index) => (&text[..index], Some(text[index..].to_string())),
            None => (text, None),
        };
        if numeric.is_empty() {
            return ComparableVersion::unparsed(text);
        }
        let mut parts = Vec::new();
        for chunk in numeric.split('.') {
            match chunk.parse::<u64>() {
                Ok(value) => parts.push(value),
                Err(_) => return ComparableVersion::unparsed(text),
            }
        }
        let calendar = parts
            .first()
            .is_some_and(|year| *year >= 24 && *year < 100 && parts.len() >= 2);
        ComparableVersion {
            calendar,
            parts,
            suffix: suffix.filter(|s| !s.is_empty()),
            unparsed: None,
        }
    }

    fn unparsed(raw: &str) -> Self {
        ComparableVersion {
            calendar: false,
            parts: Vec::new(),
            suffix: None,
            unparsed: Some(raw.to_string()),
        }
    }

    fn compare_parts(&self, other: &Self) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        for index in 0..len {
            let left = self.parts.get(index).copied().unwrap_or(0);
            let right = other.parts.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        Ordering::Equal
    }
}

impl Ord for ComparableVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Unparseable versions sort below everything else so they never win
        // "latest" selection.
        match (&self.unparsed, &other.unparsed) {
            (Some(left), Some(right)) => return left.cmp(right),
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => {}
        }
        match (self.calendar, other.calendar) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        match self.compare_parts(other) {
            Ordering::Equal => match (&self.suffix, &other.suffix) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => left.cmp(right),
            },
            other => other,
        }
    }
}

impl PartialOrd for ComparableVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Comparator {
    Any,
    Eq(ComparableVersion),
    Gt(ComparableVersion),
    Ge(ComparableVersion),
    Lt(ComparableVersion),
    Le(ComparableVersion),
    /// `~1.21.1` -> `>=1.21.1 <1.22`
    Tilde(ComparableVersion),
    /// `^1.21` -> `>=1.21 <2`
    Caret(ComparableVersion),
}

/// A Minecraft version range as declared by mod metadata.
///
/// Supported syntax (superset of Fabric's semver ranges and Maven ranges):
///
/// * `*` — anything
/// * `1.21.1`, `=1.21.1` — exact
/// * `>=1.20 <1.21`, `>=1.20,<1.21` — comparator sets (space or comma separated)
/// * `1.21.x`, `1.21.*` — wildcard
/// * `~1.21.1`, `^1.21` — tilde/caret
/// * `[1.21,1.22)` — Maven interval
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPredicate {
    raw: String,
    #[serde(skip)]
    comparators: Vec<Comparator>,
    #[serde(skip)]
    parse_error: Option<String>,
}

impl VersionPredicate {
    pub fn any() -> Self {
        VersionPredicate {
            raw: "*".into(),
            comparators: vec![Comparator::Any],
            parse_error: None,
        }
    }

    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return VersionPredicate::any();
        }
        match parse_comparators(trimmed) {
            Ok(comparators) => VersionPredicate {
                raw: trimmed.to_string(),
                comparators,
                parse_error: None,
            },
            Err(error) => VersionPredicate {
                raw: trimmed.to_string(),
                comparators: Vec::new(),
                parse_error: Some(error),
            },
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn parse_error(&self) -> Option<&str> {
        self.parse_error.as_deref()
    }

    pub fn matches(&self, version: &McVersion) -> bool {
        if self.parse_error.is_some() {
            return false;
        }
        let candidate = version.parsed();
        if candidate.unparsed.is_some() {
            return self
                .comparators
                .iter()
                .any(|comparator| matches!(comparator, Comparator::Any));
        }
        self.comparators.iter().all(|comparator| match comparator {
            Comparator::Any => true,
            Comparator::Eq(target) => candidate == *target,
            Comparator::Gt(target) => candidate > *target,
            Comparator::Ge(target) => candidate >= *target,
            Comparator::Lt(target) => candidate < *target,
            Comparator::Le(target) => candidate <= *target,
            Comparator::Tilde(target) => candidate >= *target && candidate < bump_minor(target),
            Comparator::Caret(target) => candidate >= *target && candidate < bump_major(target),
        })
    }
}

impl fmt::Display for VersionPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn bump_minor(version: &ComparableVersion) -> ComparableVersion {
    let mut parts = version.parts.clone();
    if parts.is_empty() {
        parts.push(0);
    }
    if parts.len() == 1 {
        parts.push(0);
    }
    parts[1] += 1;
    parts.truncate(2);
    ComparableVersion {
        calendar: version.calendar,
        parts,
        suffix: None,
        unparsed: None,
    }
}

fn bump_major(version: &ComparableVersion) -> ComparableVersion {
    let mut parts = version.parts.clone();
    if parts.is_empty() {
        parts.push(0);
    }
    // For `0.x` carets bump the minor component like semver does.
    if parts.first() == Some(&0) && parts.len() > 1 {
        parts[1] += 1;
        parts.truncate(2);
    } else {
        parts[0] += 1;
        parts.truncate(1);
    }
    ComparableVersion {
        calendar: version.calendar,
        parts,
        suffix: None,
        unparsed: None,
    }
}

fn parse_comparators(raw: &str) -> Result<Vec<Comparator>, String> {
    let text = raw.trim();
    if let Some(stripped) = text.strip_prefix('[').or_else(|| text.strip_prefix('(')) {
        let inclusive_low = text.starts_with('[');
        let end = stripped
            .find([']', ')'])
            .ok_or_else(|| format!("unterminated version interval `{raw}`"))?;
        let body = &stripped[..end];
        let inclusive_high = stripped.as_bytes().get(end) == Some(&b']');
        let (low, high) = body
            .split_once(',')
            .ok_or_else(|| format!("version interval `{raw}` needs two bounds"))?;
        let mut comparators = Vec::new();
        let low = low.trim();
        let high = high.trim();
        if !low.is_empty() {
            let bound = ComparableVersion::parse(low);
            comparators.push(if inclusive_low {
                Comparator::Ge(bound)
            } else {
                Comparator::Gt(bound)
            });
        }
        if !high.is_empty() {
            let bound = ComparableVersion::parse(high);
            comparators.push(if inclusive_high {
                Comparator::Le(bound)
            } else {
                Comparator::Lt(bound)
            });
        }
        return Ok(comparators);
    }

    let normalised = text.replace(',', " ");
    let mut comparators = Vec::new();
    for token in normalised.split_whitespace() {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token == "*" {
            comparators.push(Comparator::Any);
            continue;
        }
        let (operator, rest) = split_operator(token);
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(format!("missing version after operator in `{raw}`"));
        }
        // Wildcards: 1.21.x behaves like >=1.21.0 <1.22.0
        if rest.contains('x') || rest.contains('X') || rest.contains('*') {
            let cleaned = rest.replace(['x', 'X'], "0").replace('*', "0");
            let base = ComparableVersion::parse(&cleaned);
            if base.unparsed.is_some() {
                return Err(format!("invalid wildcard version `{rest}`"));
            }
            let mut parts = base.parts.clone();
            let wildcard_index = rest
                .split('.')
                .position(|part| part == "x" || part == "X" || part == "*")
                .unwrap_or(parts.len().saturating_sub(1));
            parts.truncate(wildcard_index.max(1));
            let lower = ComparableVersion {
                calendar: base.calendar,
                parts: parts.clone(),
                suffix: None,
                unparsed: None,
            };
            let mut upper_parts = parts;
            let last = upper_parts.len().saturating_sub(1);
            upper_parts[last] += 1;
            let upper = ComparableVersion {
                calendar: base.calendar,
                parts: upper_parts,
                suffix: None,
                unparsed: None,
            };
            comparators.push(Comparator::Ge(lower));
            comparators.push(Comparator::Lt(upper));
            continue;
        }
        let version = ComparableVersion::parse(rest);
        if version.unparsed.is_some() {
            return Err(format!("unparseable version `{rest}` in `{raw}`"));
        }
        let comparator = match operator {
            "" => Comparator::Eq(version),
            "=" | "==" => Comparator::Eq(version),
            ">" => Comparator::Gt(version),
            ">=" => Comparator::Ge(version),
            "<" => Comparator::Lt(version),
            "<=" => Comparator::Le(version),
            "~" => Comparator::Tilde(version),
            "^" => Comparator::Caret(version),
            other => return Err(format!("unsupported operator `{other}` in `{raw}`")),
        };
        comparators.push(comparator);
    }
    if comparators.is_empty() {
        return Err(format!("empty version range `{raw}`"));
    }
    Ok(comparators)
}

fn split_operator(token: &str) -> (&str, &str) {
    for operator in ["<=", ">=", "==", "<", ">", "=", "~", "^"] {
        if let Some(rest) = token.strip_prefix(operator) {
            return (operator, rest);
        }
    }
    ("", token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> McVersion {
        McVersion::new(raw)
    }

    #[test]
    fn calendar_versions_sort_after_legacy() {
        assert!(v("26.2") > v("1.21.11"));
        assert!(v("26.3") > v("26.2"));
        assert!(v("26.1.2") > v("26.1"));
        assert!(v("1.21.11") > v("1.21.1"));
        assert!(v("1.20.5") > v("1.20.4"));
    }

    #[test]
    fn release_candidates_sort_above_prereleases() {
        assert!(v("26.3") > v("26.3-rc-3"));
        assert!(v("26.3-rc-3") > v("26.3-pre-2"));
    }

    #[test]
    fn exact_and_comparator_ranges() {
        assert!(VersionPredicate::parse("1.21.1").matches(&v("1.21.1")));
        assert!(!VersionPredicate::parse("1.21.1").matches(&v("1.21.11")));
        let range = VersionPredicate::parse(">=1.20.1 <1.21");
        assert!(range.matches(&v("1.20.4")));
        assert!(!range.matches(&v("1.21")));
        assert!(!range.matches(&v("1.19.4")));
    }

    #[test]
    fn calendar_constraints_match_calendar_versions() {
        let range = VersionPredicate::parse(">=26.1 <26.3");
        assert!(range.matches(&v("26.2")));
        assert!(!range.matches(&v("26.3")));
        assert!(!range.matches(&v("1.21.11")));
    }

    #[test]
    fn wildcards_tilde_and_caret() {
        assert!(VersionPredicate::parse("1.21.x").matches(&v("1.21.4")));
        assert!(!VersionPredicate::parse("1.21.x").matches(&v("1.22")));
        assert!(VersionPredicate::parse("~1.21.1").matches(&v("1.21.9")));
        assert!(!VersionPredicate::parse("~1.21.1").matches(&v("1.22")));
        assert!(VersionPredicate::parse("^1.20").matches(&v("1.21.1")));
        assert!(!VersionPredicate::parse("^1.20").matches(&v("2.0")));
        assert!(VersionPredicate::parse("~26.2").matches(&v("26.2.1")));
        assert!(!VersionPredicate::parse("~26.2").matches(&v("26.3")));
    }

    #[test]
    fn maven_intervals() {
        let range = VersionPredicate::parse("[1.20,1.21)");
        assert!(range.matches(&v("1.20.6")));
        assert!(!range.matches(&v("1.21")));
        assert!(!range.matches(&v("1.19.2")));
    }

    #[test]
    fn invalid_range_reports_error_and_matches_nothing() {
        let range = VersionPredicate::parse(">=");
        assert!(range.parse_error().is_some());
        assert!(!range.matches(&v("1.21")));
    }

    #[test]
    fn any_matches_everything_including_snapshots() {
        assert!(VersionPredicate::any().matches(&v("25w14a")));
        assert!(VersionPredicate::any().matches(&v("26.3")));
    }

    #[test]
    fn snapshots_are_detected() {
        assert!(v("25w14a").is_snapshot());
        assert!(!v("26.3").is_snapshot());
        assert_eq!(v("26.2").calendar_year(), Some(26));
        assert_eq!(v("1.21.1").calendar_year(), None);
    }
}
