//! Aggregation of findings from every analyzer into one ordered, deduplicated
//! diagnostic list.

use std::collections::BTreeMap;

use mats_core::types::{Finding, FindingCategory, Severity};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: usize,
    pub error: usize,
    pub warning: usize,
    pub information: usize,
    pub not_tested: usize,
}

impl SeverityCounts {
    pub fn total(&self) -> usize {
        self.critical + self.error + self.warning + self.information + self.not_tested
    }

    pub fn blocking(&self) -> usize {
        self.critical + self.error
    }
}

/// Merge findings, combining duplicates and ordering by severity.
///
/// Duplicates are identified by `(code, detail)`. When duplicates are merged
/// the highest severity wins and evidence is concatenated (bounded), so a
/// failure reported by several analyzers appears once.
pub fn aggregate<I>(findings: I) -> Vec<Finding>
where
    I: IntoIterator<Item = Finding>,
{
    let mut merged: BTreeMap<(String, String), Finding> = BTreeMap::new();
    for finding in findings {
        let key = (
            finding.code.clone(),
            finding
                .detail
                .clone()
                .unwrap_or_else(|| finding.message.clone()),
        );
        match merged.get_mut(&key) {
            Some(existing) => {
                if finding.severity < existing.severity {
                    existing.severity = finding.severity;
                }
                for line in finding.evidence {
                    if existing.evidence.len() < 8 && !existing.evidence.contains(&line) {
                        existing.evidence.push(line);
                    }
                }
                if existing.detail.is_none() {
                    existing.detail = finding.detail;
                }
            }
            None => {
                merged.insert(key, finding);
            }
        }
    }
    let mut result: Vec<Finding> = merged.into_values().collect();
    result.sort_by(|left, right| {
        left.severity
            .cmp(&right.severity)
            .then_with(|| category_rank(left.category).cmp(&category_rank(right.category)))
            .then_with(|| left.code.cmp(&right.code))
            .then_with(|| left.message.cmp(&right.message))
    });
    result
}

fn category_rank(category: FindingCategory) -> u8 {
    match category {
        FindingCategory::Runtime => 0,
        FindingCategory::Classpath => 1,
        FindingCategory::Dependency => 2,
        FindingCategory::Mixin => 3,
        FindingCategory::Loader => 4,
        FindingCategory::Metadata => 5,
        FindingCategory::Conflict => 6,
        FindingCategory::Environment => 7,
        FindingCategory::Graphics => 8,
        FindingCategory::Network => 9,
        FindingCategory::Performance => 10,
        FindingCategory::Configuration => 11,
    }
}

pub fn count_severities(findings: &[Finding]) -> SeverityCounts {
    let mut counts = SeverityCounts::default();
    for finding in findings {
        match finding.severity {
            Severity::Critical => counts.critical += 1,
            Severity::Error => counts.error += 1,
            Severity::Warning => counts.warning += 1,
            Severity::Information => counts.information += 1,
            Severity::NotTested => counts.not_tested += 1,
        }
    }
    counts
}

/// One-line summary used by CLI output and reports.
pub fn summary_line(findings: &[Finding]) -> String {
    let counts = count_severities(findings);
    format!(
        "{} critical, {} error(s), {} warning(s), {} information",
        counts.critical, counts.error, counts.warning, counts.information
    )
}

/// Findings that must be shown prominently in `error.md`.
pub fn blocking(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|finding| matches!(finding.severity, Severity::Critical | Severity::Error))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(code: &str, severity: Severity, message: &str) -> Finding {
        Finding::new(code, severity, FindingCategory::Runtime, message)
    }

    #[test]
    fn deduplicates_and_escalates_severity() {
        let findings = vec![
            finding("runtime.x", Severity::Warning, "problem").with_detail("same"),
            finding("runtime.x", Severity::Critical, "problem")
                .with_detail("same")
                .with_evidence(["line"]),
        ];
        let merged = aggregate(findings);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].severity, Severity::Critical);
        assert_eq!(merged[0].evidence, vec!["line"]);
    }

    #[test]
    fn orders_by_severity_then_category() {
        let findings = aggregate(vec![
            finding("a", Severity::Information, "info"),
            finding("b", Severity::Critical, "critical"),
            Finding::new("c", Severity::Error, FindingCategory::Mixin, "mixin error"),
        ]);
        assert_eq!(findings[0].severity, Severity::Critical);
        assert_eq!(findings[1].severity, Severity::Error);
        assert_eq!(findings[2].severity, Severity::Information);
    }

    #[test]
    fn counts_and_blocking() {
        let findings = aggregate(vec![
            finding("a", Severity::Critical, "critical"),
            finding("b", Severity::Warning, "warning"),
            finding("c", Severity::Information, "info"),
        ]);
        let counts = count_severities(&findings);
        assert_eq!(counts.critical, 1);
        assert_eq!(counts.warning, 1);
        assert_eq!(counts.blocking(), 1);
        assert!(summary_line(&findings).contains("1 critical"));
        assert_eq!(blocking(&findings).len(), 1);
    }
}
