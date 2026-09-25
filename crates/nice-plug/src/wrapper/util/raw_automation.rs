// DELTON FORK DELTA: storage for raw parameter automation.
use nice_plug_core::context::process::ParamAutomationPoint;

// DELTON FORK DELTA
pub(crate) struct RawAutomation {
    points: Vec<ParamAutomationPoint>,
    capacity: usize,
    overflowed: bool,
}

// DELTON FORK DELTA
impl RawAutomation {
    /// Preallocates `capacity` points. `capacity` 0 is valid (flag off: nothing is stored).
    pub fn new(capacity: usize) -> Self {
        Self {
            points: Vec::with_capacity(capacity),
            capacity,
            overflowed: false,
        }
    }

    /// Empties the list and clears the overflow flag. Never frees.
    // DELTON FORK DELTA
    pub fn clear(&mut self) {
        self.points.clear();
        self.overflowed = false;
    }

    /// Appends if there is room, else sets the overflow flag and drops the point. Never grows.
    // DELTON FORK DELTA
    pub fn push(&mut self, point: ParamAutomationPoint) {
        if self.points.len() < self.capacity {
            self.points.push(point);
        } else {
            self.overflowed = true;
        }
    }

    /// Stable in-place insertion sort by `timing`. No allocation.
    // DELTON FORK DELTA
    pub fn sort_by_timing(&mut self) {
        for i in 1..self.points.len() {
            let mut j = i;
            while j > 0 && self.points[j - 1].timing > self.points[j].timing {
                self.points.swap(j - 1, j);
                j -= 1;
            }
        }
    }

    // DELTON FORK DELTA
    pub fn points(&self) -> &[ParamAutomationPoint] {
        &self.points
    }

    // DELTON FORK DELTA
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Clears `self`, then copies every point of `source` whose timing is in `start..end` into
    /// `self` with `timing - start`, preserving order. `source` must already be sorted.
    // DELTON FORK DELTA
    // Only the VST3 wrapper splits from a list it read up front; CLAP re-collects per split.
    #[cfg_attr(not(feature = "vst3"), allow(dead_code))]
    pub fn fill_rebased_window(&mut self, source: &RawAutomation, start: u32, end: u32) {
        self.clear();
        self.overflowed = source.overflowed();
        for point in source.points().iter().filter(|point| point.timing >= start) {
            if point.timing >= end {
                break;
            }
            self.push(ParamAutomationPoint {
                timing: point.timing - start,
                ..*point
            });
        }
    }
}

/// CLAP delivers parameter values in the plugin's PLAIN CLAP units: normalized * step_count for a
/// stepped parameter, the normalized value otherwise. The same conversion the wrapper applies when
/// it updates the parameter.
// DELTON FORK DELTA
pub(crate) fn clap_value_to_normalized(clap_plain_value: f64, step_count: Option<usize>) -> f32 {
    clap_plain_value as f32 / step_count.unwrap_or(1) as f32
}

// DELTON FORK DELTA
#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug_core::params::range::FloatRange;
    use nice_plug_core::params::{FloatParam, Param};

    // DELTON FORK DELTA
    fn param() -> FloatParam {
        FloatParam::new("a", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 })
    }

    // DELTON FORK DELTA
    fn point(param: &FloatParam, timing: u32, normalized_value: f32) -> ParamAutomationPoint {
        ParamAutomationPoint {
            timing,
            param: param.as_ptr(),
            normalized_value,
        }
    }

    // DELTON FORK DELTA
    #[test]
    fn push_stops_at_the_requested_capacity() {
        let param = param();
        let mut automation = RawAutomation::new(3);
        for i in 0..5 {
            automation.push(point(&param, i, i as f32 / 10.0));
        }
        assert_eq!(automation.points().len(), 3);
        assert_eq!(
            automation
                .points()
                .iter()
                .map(|p| p.timing)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            automation
                .points()
                .iter()
                .map(|p| p.normalized_value)
                .collect::<Vec<_>>(),
            [0.0, 0.1, 0.2]
        );
        assert!(
            automation
                .points()
                .iter()
                .all(|p| p.param == param.as_ptr())
        );
        assert!(automation.overflowed());
    }

    // DELTON FORK DELTA
    #[test]
    fn clear_keeps_capacity_and_resets_overflow() {
        let param = param();
        let mut automation = RawAutomation::new(3);
        for i in 0..5 {
            automation.push(point(&param, i, 0.0));
        }
        let capacity = automation.points.capacity();
        automation.clear();
        assert!(automation.points().is_empty());
        assert!(!automation.overflowed());
        assert_eq!(automation.points.capacity(), capacity);
    }

    // DELTON FORK DELTA
    #[test]
    fn capacity_zero_stores_nothing() {
        let param = param();
        let mut automation = RawAutomation::new(0);
        automation.push(point(&param, 0, 0.0));
        assert!(automation.points().is_empty());
        assert!(automation.overflowed());
    }

    // DELTON FORK DELTA
    #[test]
    fn sort_is_stable_and_by_timing() {
        let param = param();
        let mut automation = RawAutomation::new(5);
        for (timing, value) in [(5, 0.1), (1, 0.2), (5, 0.3), (0, 0.4), (1, 0.5)] {
            automation.push(point(&param, timing, value));
        }
        automation.sort_by_timing();
        assert_eq!(
            automation
                .points()
                .iter()
                .map(|p| p.timing)
                .collect::<Vec<_>>(),
            [0, 1, 1, 5, 5]
        );
        assert_eq!(
            automation
                .points()
                .iter()
                .map(|p| p.normalized_value)
                .collect::<Vec<_>>(),
            [0.4, 0.2, 0.5, 0.1, 0.3]
        );
    }

    // DELTON FORK DELTA
    #[test]
    fn sorting_does_not_allocate() {
        let param = param();
        let mut automation = RawAutomation::new(3);
        let capacity = automation.points.capacity();
        for timing in [2, 1, 0] {
            automation.push(point(&param, timing, 0.0));
        }
        automation.sort_by_timing();
        assert_eq!(automation.points.capacity(), capacity);
    }

    // DELTON FORK DELTA
    #[test]
    fn rebased_window_selects_and_rebases() {
        let param = param();
        let mut source = RawAutomation::new(4);
        for (timing, value) in [(0, 0.0), (10, 0.1), (20, 0.2), (30, 0.3)] {
            source.push(point(&param, timing, value));
        }
        let mut target = RawAutomation::new(4);
        target.fill_rebased_window(&source, 10, 30);
        assert_eq!(
            target.points().iter().map(|p| p.timing).collect::<Vec<_>>(),
            [0, 10]
        );
        assert_eq!(
            target
                .points()
                .iter()
                .map(|p| p.normalized_value)
                .collect::<Vec<_>>(),
            [0.1, 0.2]
        );
    }

    // DELTON FORK DELTA
    #[test]
    fn rebased_window_propagates_and_detects_overflow() {
        let param = param();
        let mut source = RawAutomation::new(2);
        source.push(point(&param, 10, 0.1));
        source.push(point(&param, 20, 0.2));
        let mut target = RawAutomation::new(1);
        target.fill_rebased_window(&source, 0, 30);
        assert_eq!(target.points().len(), 1);
        assert!(target.overflowed());

        source.push(point(&param, 40, 0.4));
        target.fill_rebased_window(&source, 10, 20);
        assert_eq!(target.points().len(), 1);
        assert!(target.overflowed());
    }

    // DELTON FORK DELTA
    #[test]
    fn clap_values_are_normalized_by_step_count() {
        assert_eq!(clap_value_to_normalized(2.0, Some(4)), 0.5);
        assert_eq!(clap_value_to_normalized(0.25, None), 0.25);
        assert_eq!(clap_value_to_normalized(4.0, Some(4)), 1.0);
    }
}
