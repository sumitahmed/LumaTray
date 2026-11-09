use std::collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPoint {
    pub input: f64,   // 0.0 ..= 100.0 (e.g. Master Slider / UI visual level)
    pub output: f64,  // 0.0 ..= 100.0 (e.g. Actual hardware level applied)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiecewiseCurve {
    pub min_output: f64,
    pub max_output: f64,
    pub points: Vec<CalibrationPoint>,
}

impl Default for PiecewiseCurve {
    fn default() -> Self {
        Self {
            min_output: 0.0,
            max_output: 100.0,
            points: vec![
                CalibrationPoint { input: 0.0, output: 0.0 },
                CalibrationPoint { input: 100.0, output: 100.0 },
            ],
        }
    }
}

impl PiecewiseCurve {
    pub fn new(min_output: f64, max_output: f64, custom_points: Vec<CalibrationPoint>) -> Self {
        let mut points = custom_points;

        // Ensure (0, min_output) and (100, max_output) are present
        let mut has_start = false;
        let mut has_end = false;

        for p in &points {
            if (p.input - 0.0).abs() < f64::EPSILON {
                has_start = true;
            }
            if (p.input - 100.0).abs() < f64::EPSILON {
                has_end = true;
            }
        }

        if !has_start {
            points.push(CalibrationPoint {
                input: 0.0,
                output: min_output.clamp(0.0, 100.0),
            });
        }
        if !has_end {
            points.push(CalibrationPoint {
                input: 100.0,
                output: max_output.clamp(0.0, 100.0),
            });
        }

        // Sort ascending by input
        points.sort_by(|a, b| a.input.partial_cmp(&b.input).unwrap_or(std::cmp::Ordering::Equal));

        Self {
            min_output: min_output.clamp(0.0, 100.0),
            max_output: max_output.clamp(0.0, 100.0),
            points,
        }
    }

    /// Forward Mapping: Master visual input (0..=100) -> Hardware level output (0..=100)
    pub fn map_forward(&self, input: f64) -> f64 {
        let input = input.clamp(0.0, 100.0);

        if self.points.is_empty() {
            return input;
        }
        if self.points.len() == 1 {
            return self.points[0].output;
        }

        // Exact match check for first point
        if input <= self.points[0].input {
            return self.points[0].output;
        }
        // Exact match check for last point
        let last = self.points.last().unwrap();
        if input >= last.input {
            return last.output;
        }

        // Find surrounding interval [p1, p2]
        for window in self.points.windows(2) {
            let p1 = window[0];
            let p2 = window[1];

            if input >= p1.input && input <= p2.input {
                let range_in = p2.input - p1.input;
                if range_in.abs() < f64::EPSILON {
                    return p1.output;
                }
                let ratio = (input - p1.input) / range_in;
                let output = p1.output + ratio * (p2.output - p1.output);
                return output.clamp(0.0, 100.0);
            }
        }

        input
    }

    /// Reverse Mapping: Hardware level (0..=100) -> Master visual level (0..=100)
    pub fn map_reverse(&self, output: f64) -> f64 {
        let output = output.clamp(0.0, 100.0);

        if self.points.is_empty() {
            return output;
        }
        if self.points.len() == 1 {
            return self.points[0].input;
        }

        for window in self.points.windows(2) {
            let p1 = window[0];
            let p2 = window[1];

            let min_out = p1.output.min(p2.output);
            let max_out = p1.output.max(p2.output);

            if output >= min_out && output <= max_out {
                let range_out = p2.output - p1.output;
                if range_out.abs() < f64::EPSILON {
                    return (p1.input + p2.input) / 2.0;
                }
                let ratio = (output - p1.output) / range_out;
                let input = p1.input + ratio * (p2.input - p1.input);
                return input.clamp(0.0, 100.0);
            }
        }

        output
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplaySyncEngine {
    pub linked_mode: bool,
    pub master_display_id: Option<String>,
    pub curves: HashMap<String, PiecewiseCurve>,
}

impl Default for DisplaySyncEngine {
    fn default() -> Self {
        Self {
            linked_mode: false,
            master_display_id: None,
            curves: HashMap::new(),
        }
    }
}

impl DisplaySyncEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_curve(&mut self, id: String, curve: PiecewiseCurve) {
        self.curves.insert(id, curve);
    }

    pub fn get_curve(&self, id: &str) -> PiecewiseCurve {
        self.curves.get(id).cloned().unwrap_or_default()
    }

    /// Calculates synchronized hardware levels for all monitors given a master level.
    pub fn calculate_synced_levels(
        &self,
        display_ids: &[String],
        master_level: u32,
    ) -> HashMap<String, u32> {
        let mut results = HashMap::new();
        let master_f = master_level as f64;

        for id in display_ids {
            let curve = self.get_curve(id);
            let target_f = curve.map_forward(master_f);
            results.insert(id.clone(), target_f.round() as u32);
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_curve_identity() {
        let curve = PiecewiseCurve::default();
        assert_eq!(curve.map_forward(0.0), 0.0);
        assert_eq!(curve.map_forward(50.0), 50.0);
        assert_eq!(curve.map_forward(100.0), 100.0);

        assert_eq!(curve.map_reverse(50.0), 50.0);
    }

    #[test]
    fn test_custom_min_max_curve() {
        // Monitor has minimum hardware brightness threshold of 15%
        let curve = PiecewiseCurve::new(15.0, 100.0, vec![]);
        assert_eq!(curve.map_forward(0.0), 15.0);
        assert_eq!(curve.map_forward(50.0), 57.5);
        assert_eq!(curve.map_forward(100.0), 100.0);

        // Reverse map 15% hardware -> 0% visual
        assert_eq!(curve.map_reverse(15.0), 0.0);
        assert_eq!(curve.map_reverse(100.0), 100.0);
    }

    #[test]
    fn test_multi_point_piecewise_curve() {
        // Non-linear calibration: at visual 50%, output should be 70%
        let curve = PiecewiseCurve::new(
            0.0,
            100.0,
            vec![
                CalibrationPoint { input: 0.0, output: 0.0 },
                CalibrationPoint { input: 50.0, output: 70.0 },
                CalibrationPoint { input: 100.0, output: 100.0 },
            ],
        );

        // At 25% visual -> 35% hardware
        assert_eq!(curve.map_forward(25.0), 35.0);
        // At 50% visual -> 70% hardware
        assert_eq!(curve.map_forward(50.0), 70.0);
        // At 75% visual -> 85% hardware
        assert_eq!(curve.map_forward(75.0), 85.0);

        // Reverse test
        assert_eq!(curve.map_reverse(70.0), 50.0);
    }
}
