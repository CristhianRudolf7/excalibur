//! Calibration measured on Excalibur's own labeled decisions.
//!
//! Every sample is (probability that the proposition holds, observed outcome).
//! For predicates the proposition is the statement; for choice and score
//! answers it is "the selected option or level is correct".
use crate::{DeciderError, Result, Thresholds};
use serde::{Deserialize, Serialize};

pub const BINS: usize = 10;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct LabeledOutcome {
    pub probability: f64,
    pub outcome: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReliabilityBin {
    pub lower: f64,
    pub upper: f64,
    pub count: usize,
    pub mean_probability: Option<f64>,
    pub observed_frequency: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CalibrationReport {
    pub samples: usize,
    /// Expected calibration error over ten equal-width bins.
    pub ece: f64,
    /// Maximum calibration error over nonempty bins.
    pub mce: f64,
    pub brier: f64,
    /// Reliability curve.
    pub bins: Vec<ReliabilityBin>,
    pub target_precision: f64,
    pub min_support: usize,
    pub thresholds: Thresholds,
    /// Fraction of samples with p >= tau_high and their observed precision.
    pub act_coverage: f64,
    pub act_precision: Option<f64>,
    /// Fraction of samples with p <= tau_low and the observed fraction that were false.
    pub alternative_coverage: f64,
    pub alternative_precision: Option<f64>,
    /// Fraction routed to the agent (the abstention band).
    pub deferred_coverage: f64,
}

/// Measure calibration and select the widest contiguous action regions whose
/// observed precision meets `target_precision` with at least `min_support` samples.
pub fn calibrate(
    samples: &[LabeledOutcome],
    target_precision: f64,
    min_support: usize,
) -> Result<CalibrationReport> {
    if !(0.5..1.0).contains(&target_precision) || min_support == 0 {
        return Err(DeciderError::Invalid(
            "target precision must be in [0.5, 1) with positive support".into(),
        ));
    }
    if samples.is_empty()
        || samples
            .iter()
            .any(|s| !s.probability.is_finite() || !(0.0..=1.0).contains(&s.probability))
    {
        return Err(DeciderError::Invalid(
            "calibration needs labeled probabilities in [0, 1]".into(),
        ));
    }
    let n = samples.len() as f64;
    let mut bins = (0..BINS)
        .map(|i| ReliabilityBin {
            lower: i as f64 / BINS as f64,
            upper: (i + 1) as f64 / BINS as f64,
            count: 0,
            mean_probability: None,
            observed_frequency: None,
        })
        .collect::<Vec<_>>();
    let mut sums = vec![(0.0, 0.0); BINS];
    let mut brier = 0.0;
    for s in samples {
        let index = ((s.probability * BINS as f64) as usize).min(BINS - 1);
        bins[index].count += 1;
        sums[index].0 += s.probability;
        sums[index].1 += f64::from(u8::from(s.outcome));
        brier += (s.probability - f64::from(u8::from(s.outcome))).powi(2);
    }
    let mut ece = 0.0;
    let mut mce: f64 = 0.0;
    for (bin, (p, o)) in bins.iter_mut().zip(sums) {
        if bin.count > 0 {
            let c = bin.count as f64;
            bin.mean_probability = Some(p / c);
            bin.observed_frequency = Some(o / c);
            let gap = (p / c - o / c).abs();
            ece += gap * c / n;
            mce = mce.max(gap);
        }
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.probability.total_cmp(&b.probability));
    let candidates = {
        let mut values = sorted.iter().map(|s| s.probability).collect::<Vec<_>>();
        values.dedup();
        values
    };
    // tau_high: descend while {p >= t} keeps the target precision.
    let mut tau_high = None;
    for t in candidates.iter().rev().copied() {
        let selected = sorted.iter().filter(|s| s.probability >= t);
        let (count, positives) = selected.fold((0usize, 0usize), |(c, p), s| {
            (c + 1, p + usize::from(s.outcome))
        });
        if count < min_support {
            continue;
        }
        if positives as f64 / count as f64 >= target_precision {
            tau_high = Some(t);
        } else {
            break;
        }
    }
    let mut tau_low = None;
    for t in candidates.iter().copied() {
        let (count, negatives) = sorted
            .iter()
            .filter(|s| s.probability <= t)
            .fold((0usize, 0usize), |(c, n), s| {
                (c + 1, n + usize::from(!s.outcome))
            });
        if count < min_support {
            continue;
        }
        if negatives as f64 / count as f64 >= target_precision {
            tau_low = Some(t);
        } else {
            break;
        }
    }
    if let (Some(low), Some(high)) = (tau_low, tau_high)
        && low >= high
    {
        tau_low = None;
    }
    let thresholds = Thresholds {
        tau_low,
        tau_high,
        calibrated: true,
    };
    let region = |pred: &dyn Fn(f64) -> bool, positive: bool| {
        let (count, hits) = samples
            .iter()
            .filter(|s| pred(s.probability))
            .fold((0usize, 0usize), |(c, h), s| {
                (c + 1, h + usize::from(s.outcome == positive))
            });
        (
            count as f64 / n,
            (count > 0).then(|| hits as f64 / count as f64),
        )
    };
    let (act_coverage, act_precision) = region(&|p| tau_high.is_some_and(|h| p >= h), true);
    let (alternative_coverage, alternative_precision) =
        region(&|p| tau_low.is_some_and(|l| p <= l), false);
    Ok(CalibrationReport {
        samples: samples.len(),
        ece,
        mce,
        brier: brier / n,
        bins,
        target_precision,
        min_support,
        thresholds,
        act_coverage,
        act_precision,
        alternative_coverage,
        alternative_precision,
        deferred_coverage: (1.0 - act_coverage - alternative_coverage).max(0.0),
    })
}
