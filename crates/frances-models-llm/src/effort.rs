use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};

/// Provider-neutral reasoning effort as an integer percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct NormalizedEffort(u8);

impl NormalizedEffort {
    pub const MIN: u8 = 0;
    pub const MAX: u8 = 100;

    pub fn new(value: u8) -> Result<Self, InvalidNormalizedEffort> {
        if value <= Self::MAX {
            Ok(Self(value))
        } else {
            Err(InvalidNormalizedEffort(value))
        }
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for NormalizedEffort {
    type Error = InvalidNormalizedEffort;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NormalizedEffort> for u8 {
    fn from(value: NormalizedEffort) -> Self {
        value.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("effort must be an integer from 0 through 100, got {0}")]
pub struct InvalidNormalizedEffort(u8);

/// Ordered provider labels for normalized effort percentages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortTiers(Vec<String>);

impl EffortTiers {
    pub fn new(labels: Vec<String>) -> Result<Self, String> {
        if labels.len() < 2 {
            return Err("effort_tiers must contain at least two labels".to_owned());
        }
        if labels.iter().any(|label| label.trim().is_empty()) {
            return Err("effort_tiers labels must not be empty".to_owned());
        }
        let mut unique = HashSet::with_capacity(labels.len());
        if labels.iter().any(|label| !unique.insert(label.as_str())) {
            return Err("effort_tiers labels must be unique".to_owned());
        }
        Ok(Self(labels))
    }

    pub fn openai() -> Self {
        Self(
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    pub fn labels(&self) -> &[String] {
        &self.0
    }

    pub fn label_for(&self, effort: NormalizedEffort) -> &str {
        let target = effort.get() as usize;
        let points = tier_points(self.0.len());
        let mut best = 0;
        let mut best_distance = target.abs_diff(points[0]);
        for (index, point) in points.into_iter().enumerate().skip(1) {
            let distance = target.abs_diff(point);
            if distance <= best_distance {
                best = index;
                best_distance = distance;
            }
        }
        &self.0[best]
    }
}

fn tier_points(label_count: usize) -> Vec<usize> {
    let interior_count = label_count - 2;
    let mut points = Vec::with_capacity(label_count);
    points.push(0);
    match interior_count {
        0 => {}
        1 => points.push(50),
        count => {
            for index in 0..count {
                points.push(1 + (index * 98 + (count - 1) / 2) / (count - 1));
            }
        }
    }
    points.push(100);
    points
}

impl<'de> Deserialize<'de> for EffortTiers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Preset(String),
            Labels(Vec<String>),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Preset(preset) if preset == "openai" => Ok(Self::openai()),
            Raw::Preset(preset) => Err(serde::de::Error::custom(format!(
                "unknown effort_tiers preset {preset:?}"
            ))),
            Raw::Labels(labels) => Self::new(labels).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EffortTiers, NormalizedEffort, tier_points};

    #[test]
    fn accepts_inclusive_percentage_range() {
        assert_eq!(NormalizedEffort::new(0).unwrap().get(), 0);
        assert_eq!(NormalizedEffort::new(50).unwrap().get(), 50);
        assert_eq!(NormalizedEffort::new(100).unwrap().get(), 100);
        assert!(NormalizedEffort::new(101).is_err());
    }

    #[test]
    fn serde_rejects_out_of_range_percentage() {
        assert_eq!(
            serde_json::from_str::<NormalizedEffort>("100").unwrap(),
            NormalizedEffort::new(100).unwrap()
        );
        assert!(serde_json::from_str::<NormalizedEffort>("101").is_err());
        assert!(serde_json::from_str::<NormalizedEffort>("-1").is_err());
        assert!(serde_json::from_str::<NormalizedEffort>("1.5").is_err());
    }

    #[test]
    fn openai_tiers_have_reserved_and_evenly_distributed_points() {
        let tiers = EffortTiers::openai();
        assert_eq!(
            tier_points(tiers.labels().len()),
            [0, 1, 26, 50, 75, 99, 100]
        );
        assert_eq!(tiers.label_for(NormalizedEffort::new(0).unwrap()), "none");
        assert_eq!(tiers.label_for(NormalizedEffort::new(14).unwrap()), "low");
        assert_eq!(tiers.label_for(NormalizedEffort::new(100).unwrap()), "max");
    }

    #[test]
    fn nearest_tier_ties_resolve_upward() {
        let tiers = EffortTiers::new(vec!["off".into(), "mid".into(), "full".into()]).unwrap();
        assert_eq!(tier_points(3), [0, 50, 100]);
        assert_eq!(tiers.label_for(NormalizedEffort::new(25).unwrap()), "mid");
        assert_eq!(tiers.label_for(NormalizedEffort::new(75).unwrap()), "full");
    }

    #[test]
    fn deserialization_validates_presets_and_labels() {
        let preset: EffortTiers = serde_json::from_str("\"openai\"").unwrap();
        assert_eq!(preset, EffortTiers::openai());

        let labels: EffortTiers = serde_json::from_str(r#"["Off","MAX"]"#).unwrap();
        assert_eq!(labels.labels(), ["Off", "MAX"]);
        assert!(serde_json::from_str::<EffortTiers>("\"anthropic\"").is_err());
        assert!(serde_json::from_str::<EffortTiers>(r#"["only"]"#).is_err());
        assert!(serde_json::from_str::<EffortTiers>(r#"["", "high"]"#).is_err());
        assert!(serde_json::from_str::<EffortTiers>(r#"["low", "low"]"#).is_err());
    }
}
