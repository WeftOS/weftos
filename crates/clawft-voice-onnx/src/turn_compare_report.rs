//! Aggregate report for the Smart Turn offline comparison: report maths, JSON
//! and a short Markdown summary.

use serde_json::{Value, json};

use crate::turn_compare::{CompareConfig, FileResult};

/// Nearest-rank percentile of `values` (`p` in `(0, 100]`). `None` when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() || !(p > 0.0 && p <= 100.0) {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

/// Totals across all replayed files.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub files: usize,
    pub pauses: usize,
    pub agreements: usize,
    pub premature_cut_ins: usize,
    pub confirmed_cut_ins: usize,
    pub late_ends: usize,
    pub skipped_short_pauses: usize,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
}

impl Summary {
    pub fn from_files(files: &[FileResult]) -> Self {
        let pauses = || files.iter().flat_map(|f| f.pauses.iter());
        let latencies: Vec<f64> = files
            .iter()
            .flat_map(|f| f.latencies_ms.iter().copied())
            .collect();
        Self {
            files: files.len(),
            pauses: pauses().count(),
            agreements: pauses().filter(|p| p.agree).count(),
            premature_cut_ins: pauses().filter(|p| p.premature_cut_in).count(),
            confirmed_cut_ins: pauses().filter(|p| p.confirmed_cut_in).count(),
            late_ends: pauses().filter(|p| p.late_end).count(),
            skipped_short_pauses: files.iter().map(|f| f.skipped_short_pauses).sum(),
            latency_p50_ms: percentile(&latencies, 50.0),
            latency_p95_ms: percentile(&latencies, 95.0),
        }
    }

    /// Fraction of candidate pauses where both verdicts match; `None` with no pauses.
    pub fn agreement_rate(&self) -> Option<f64> {
        (self.pauses > 0).then(|| self.agreements as f64 / self.pauses as f64)
    }
}

/// Full JSON report.
pub fn to_json(files: &[FileResult], cfg: &CompareConfig, model: &str) -> Value {
    let s = Summary::from_files(files);
    json!({
        "model": model,
        "config": cfg.to_json(),
        "summary": {
            "files": s.files,
            "candidate_pauses": s.pauses,
            "skipped_short_pauses": s.skipped_short_pauses,
            "agreements": s.agreements,
            "agreement_rate": s.agreement_rate(),
            "premature_cut_ins": s.premature_cut_ins,
            "confirmed_cut_ins": s.confirmed_cut_ins,
            "late_ends": s.late_ends,
            "latency_p50_ms": s.latency_p50_ms,
            "latency_p95_ms": s.latency_p95_ms,
        },
        "files": files.iter().map(|f| json!({
            "file": f.file,
            "duration_ms": f.duration_ms,
            "skipped_short_pauses": f.skipped_short_pauses,
            "pauses": f.pauses.iter().map(|p| p.to_json()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn fmt_opt(v: Option<f64>, unit: &str) -> String {
    v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.1}{unit}"))
}

/// Short Markdown summary.
pub fn to_markdown(files: &[FileResult], cfg: &CompareConfig, model: &str) -> String {
    let s = Summary::from_files(files);
    let mut md = String::new();
    md.push_str("# Smart Turn offline comparison\n\n");
    md.push_str(&format!(
        "Model: `{model}`. Threshold {}, pause {} ms, max silence {} ms, probe step {} ms.\n\n",
        cfg.threshold, cfg.short_silence_ms, cfg.max_silence_ms, cfg.step_ms
    ));
    md.push_str("\"Ours\" is the silence-only gate (finalize at the first qualifying pause).\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    md.push_str(&format!("| Files | {} |\n", s.files));
    md.push_str(&format!("| Candidate pauses | {} |\n", s.pauses));
    md.push_str(&format!(
        "| Agreement rate | {} |\n",
        fmt_opt(s.agreement_rate().map(|r| r * 100.0), "%")
    ));
    md.push_str(&format!(
        "| Premature cut-ins (ours ended, model incomplete) | {} |\n",
        s.premature_cut_ins
    ));
    md.push_str(&format!(
        "| Confirmed cut-ins (speaker resumed) | {} |\n",
        s.confirmed_cut_ins
    ));
    md.push_str(&format!("| Late ends | {} |\n", s.late_ends));
    md.push_str(&format!(
        "| Pauses below threshold length (skipped) | {} |\n",
        s.skipped_short_pauses
    ));
    md.push_str(&format!(
        "| Inference p50 | {} |\n",
        fmt_opt(s.latency_p50_ms, " ms")
    ));
    md.push_str(&format!(
        "| Inference p95 | {} |\n\n",
        fmt_opt(s.latency_p95_ms, " ms")
    ));
    if s.pauses == 0 {
        md.push_str("No candidate pauses were found; check the audio and the pause length.\n");
        return md;
    }
    md.push_str("## Premature cut-ins\n\n");
    let cuts: Vec<_> = files
        .iter()
        .flat_map(|f| f.pauses.iter())
        .filter(|p| p.premature_cut_in)
        .collect();
    if cuts.is_empty() {
        md.push_str("None.\n");
    } else {
        md.push_str(
            "| File | Speech end | Silence | Model prob | Resumed |\n|---|---|---|---|---|\n",
        );
        for p in cuts {
            md.push_str(&format!(
                "| {} | {} ms | {} ms | {:.2} | {} |\n",
                p.file,
                p.speech_end_ms,
                p.silence_ms,
                p.model_prob,
                if p.resumed { "yes" } else { "no" }
            ));
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_compare::PauseRecord;

    #[test]
    fn percentile_nearest_rank() {
        let v = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&v, 50.0), Some(20.0));
        assert_eq!(percentile(&v, 95.0), Some(40.0));
        assert_eq!(percentile(&v, 100.0), Some(40.0));
        assert_eq!(percentile(&[5.0], 50.0), Some(5.0));
    }

    #[test]
    fn percentile_refuses_empty_and_bad_rank() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[1.0], 0.0), None);
        assert_eq!(percentile(&[1.0], 101.0), None);
    }

    fn pause(agree: bool, premature: bool, confirmed: bool, late: bool) -> PauseRecord {
        PauseRecord {
            file: "a.wav".into(),
            index: 0,
            speech_end_ms: 1000,
            silence_ms: 700,
            turn_voiced_ms: 800,
            resumed: confirmed,
            ours_finalized: true,
            ours_end_silence_ms: Some(500),
            model_prob: 0.2,
            model_complete: false,
            model_end_silence_ms: None,
            agree,
            premature_cut_in: premature,
            confirmed_cut_in: confirmed,
            late_end: late,
            latency_ms: 10.0,
        }
    }

    #[test]
    fn summary_counts_and_rate() {
        let f = FileResult {
            file: "a.wav".into(),
            duration_ms: 5000,
            skipped_short_pauses: 2,
            pauses: vec![
                pause(true, false, false, false),
                pause(false, true, true, false),
                pause(false, true, false, true),
                pause(true, false, false, false),
            ],
            latencies_ms: vec![1.0, 2.0, 3.0, 4.0, 100.0],
        };
        let s = Summary::from_files(&[f]);
        assert_eq!((s.pauses, s.agreements), (4, 2));
        assert_eq!(
            (s.premature_cut_ins, s.confirmed_cut_ins, s.late_ends),
            (2, 1, 1)
        );
        assert_eq!(s.skipped_short_pauses, 2);
        assert_eq!(s.agreement_rate(), Some(0.5));
        assert_eq!(s.latency_p50_ms, Some(3.0));
        assert_eq!(s.latency_p95_ms, Some(100.0));
    }

    #[test]
    fn empty_run_has_no_rate_and_says_so() {
        let s = Summary::from_files(&[]);
        assert_eq!(s.agreement_rate(), None);
        let cfg = CompareConfig::default();
        assert!(to_markdown(&[], &cfg, "m").contains("No candidate pauses"));
        assert!(to_json(&[], &cfg, "m")["summary"]["agreement_rate"].is_null());
    }
}
