use crate::config::{Band, Config, MAX_BANDS, MAX_FREQUENCY, MIN_FREQUENCY};
use serde::{Deserialize, Serialize};

const SAMPLES: usize = 1025;
pub const TOLERANCE_DB: f64 = 0.15;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Target {
    rate: u32,
    millidb: Vec<i32>,
}

impl Target {
    pub fn valid(&self) -> bool {
        self.rate > 40 && self.millidb.len() == SAMPLES
            && self.millidb.iter().all(|value| (-24000..=24000).contains(value))
    }
}

pub fn convert(mut config: Config, rate: u32) -> (Config, f64) {
    let frequencies: Vec<_> = (0..SAMPLES).map(|index| {
        MIN_FREQUENCY * 1000.0_f64.powf(index as f64 / (SAMPLES - 1) as f64)
    }).collect();
    let rate = rate.max(41);
    let nyquist = f64::from(rate) / 2.0;
    let audible: Vec<_> = frequencies.iter().map(|frequency| frequency.min(nyquist)).collect();
    let direct_spline = config.spline_points().is_some() && config.spline_source.is_none()
        && config.parametric_bands().is_empty();
    let target = config.fit_target.take().filter(|target| target.rate == rate).unwrap_or_else(|| Target {
        rate,
        millidb: config.response(if direct_spline { &frequencies } else { &audible }.iter().copied(), f64::from(rate)).into_iter()
            .map(|gain| (gain.clamp(-24.0, 24.0) * 1000.0).round() as i32).collect(),
    });
    let db: Vec<_> = target.millidb.iter().map(|gain| f64::from(*gain) / 1000.0).collect();
    config.spline = !config.spline;
    config.bands = if config.spline {
        fit_spline(&frequencies, &db)
    } else {
        fit_parametric(&audible, &db, f64::from(rate))
    };
    config.spline_source = None;
    config.other_mode_bands = None;
    config.fit_target = Some(target);
    config.sort_bands();
    let actual = config.response(if config.spline { frequencies } else { audible }, f64::from(rate));
    let error = max_error(&actual, &db);
    (config, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(kind: &str, frequency_hz: f64, gain_db: f64, q: f64) -> Band {
        Band { kind: kind.into(), frequency_hz, gain_db, q, ..Band::flat(0, 1) }
    }

    #[test]
    fn flat_fits_use_the_smallest_models() {
        let (spline, error) = convert(Config::default(), 48000);
        assert_eq!(spline.bands.len(), 2);
        assert_eq!(error, 0.0);
        let (parametric, error) = convert(spline, 48000);
        assert!(parametric.bands.is_empty());
        assert_eq!(error, 0.0);
    }

    #[test]
    fn narrow_peak_fits_back_to_one_filter_and_does_not_drift_after_reloads() {
        let input = Config { bands: vec![band("PK", 1129.0, -11.0, 14.0)], ..Config::default() };
        let (spline, spline_error) = convert(input, 48000);
        // This notch is narrower than a single editable spline segment.
        assert!(spline_error > TOLERANCE_DB && spline_error < 5.0, "{spline_error}");
        assert!(spline.bands.windows(2).all(|pair| {
            crate::spline::points_are_spaced(pair[0].frequency_hz, pair[1].frequency_hz)
        }));
        assert!(spline.bands.len() < MAX_BANDS);
        assert!(spline.other_mode_bands.is_none());
        let (parametric, parametric_error) = convert(spline.clone(), 48000);
        assert!(parametric_error <= TOLERANCE_DB, "{parametric_error}");
        assert_eq!(parametric.bands.len(), 1);

        let loaded = Config::from_json(&serde_json::to_string(&parametric).unwrap()).unwrap();
        let (again, error) = convert(loaded, 48000);
        assert_eq!(again.bands, spline.bands);
        assert_eq!(error, spline_error);
        let (again, _) = convert(again, 48000);
        assert_eq!(again.bands, parametric.bands);
    }

    #[test]
    fn fits_a_composite_curve_without_keeping_a_hidden_filter() {
        let input = Config { bands: vec![
            band("LS", 115.0, 5.0, 0.7),
            band("PK", 910.0, -4.0, 1.4),
            band("HS", 8000.0, 2.0, 0.8),
        ], ..Config::default() };
        let (spline, error) = convert(input, 48000);
        assert!(spline.parametric_bands().is_empty() && spline.spline_source.is_none());
        assert!(error <= TOLERANCE_DB, "{error}");
        let (parametric, error) = convert(spline, 48000);
        assert!(parametric.spline_points().is_none());
        assert!(error <= TOLERANCE_DB, "{error}");
        assert_eq!(parametric.bands.len(), 3);
    }
}

fn max_error(actual: &[f64], target: &[f64]) -> f64 {
    actual.iter().zip(target).map(|(actual, target)| (actual - target).abs()).fold(0.0, f64::max)
}

// Shortest path through the sampled knots. Every admissible edge represents a
// spline segment inside the error limit. Ascending indices resolve tied paths.
fn spline_knots(db: &[f64], tolerance: f64) -> Option<Vec<usize>> {
    let mut counts = vec![usize::MAX; db.len()];
    let mut predecessors = vec![0; db.len()];
    counts[0] = 1;
    let min_gap = (crate::spline::MIN_POINT_GAP * (db.len() - 1) as f64).ceil() as usize;
    for right in min_gap..db.len() {
        for left in 0..=right - min_gap {
            if counts[left] >= MAX_BANDS || counts[left] + 1 >= counts[right] {
                continue;
            }
            let gain = db[left];
            let delta = db[right] - gain;
            let span = (right - left) as f64;
            if db[left + 1..right].iter().enumerate().all(|(offset, &wanted)| {
                let t = (offset + 1) as f64 / span;
                (wanted - (gain + delta * t * t * (3.0 - 2.0 * t))).abs() <= tolerance
            }) {
                counts[right] = counts[left] + 1;
                predecessors[right] = left;
            }
        }
    }
    let mut index = db.len() - 1;
    if counts[index] == usize::MAX {
        return None;
    }
    let mut knots = vec![index];
    while index != 0 {
        index = predecessors[index];
        knots.push(index);
    }
    knots.reverse();
    Some(knots)
}

fn fit_spline(frequencies: &[f64], db: &[f64]) -> Vec<Band> {
    let knots = spline_knots(db, TOLERANCE_DB).unwrap_or_else(|| {
        // Find a feasible error bound while keeping the 32-point budget.
        let (mut lower, mut upper) = (TOLERANCE_DB, 48.0);
        let mut best = vec![0, db.len() - 1];
        for _ in 0..12 {
            let middle = (lower + upper) * 0.5;
            if let Some(knots) = spline_knots(db, middle) {
                best = knots;
                upper = middle;
            } else {
                lower = middle;
            }
        }
        best
    });
    knots.into_iter().map(|index| Band {
        frequency_hz: frequencies[index], gain_db: db[index], edited: Some(true), ..Band::flat(0, 1)
    }).collect()
}

struct ParametricFit<'a> {
    frequencies: &'a [f64],
    grid: resonance_ipc::fr::ResponseGrid,
    max_frequency: f64,
}

impl ParametricFit<'_> {
    fn response(&self, band: &Band) -> Vec<f64> {
        self.grid.band_response(&band.state())
    }

    fn band(&self, kind: &str, values: [f64; 3]) -> Band {
        Band {
            kind: kind.into(),
            frequency_hz: values[0].exp().clamp(MIN_FREQUENCY, self.max_frequency),
            gain_db: (values[1] * 12.0).clamp(-24.0, 24.0),
            q: values[2].exp().clamp(0.1, 20.0),
            edited: Some(true),
            ..Band::flat(0, 1)
        }
    }

    fn refine(&self, seed: Band, target: &[f64], steps: usize) -> (Band, Vec<f64>) {
        let parameters = |band: &Band| [band.frequency_hz.ln(), band.gain_db / 12.0, band.q.ln()];
        let mut band = seed;
        let mut response = self.response(&band);
        let mut score = cost(&response, target);
        let mut damping = 0.01;
        for _ in 0..steps {
            if max_error(&response, target) < 0.003 {
                break;
            }
            let coordinates = parameters(&band);
            let mut derivatives = Vec::with_capacity(3);
            for axis in 0..3 {
                let mut upper = coordinates;
                let mut lower = coordinates;
                upper[axis] += 0.001;
                lower[axis] -= 0.001;
                let upper = self.band(&band.kind, upper);
                let lower = self.band(&band.kind, lower);
                let span = parameters(&upper)[axis] - parameters(&lower)[axis];
                let upper = self.response(&upper);
                let lower = self.response(&lower);
                derivatives.push(upper.into_iter().zip(lower).map(|(upper, lower)| (upper - lower) / span).collect::<Vec<_>>());
            }
            let mut system = [[0.0; 4]; 3];
            for index in 0..response.len() {
                let error = target[index] - response[index];
                let weight = 1.0 + 0.2 * error * error;
                for row in 0..3 {
                    let derivative = derivatives[row][index];
                    for col in 0..3 {
                        system[row][col] += weight * derivative * derivatives[col][index];
                    }
                    system[row][3] += weight * derivative * error;
                }
            }
            for (axis, row) in system.iter_mut().enumerate() {
                row[axis] += damping * row[axis].max(1.0);
            }
            let step = solve_three(system);
            let trial = self.band(&band.kind, std::array::from_fn(|axis| coordinates[axis] + step[axis].clamp(-0.65, 0.65)));
            let next = self.response(&trial);
            let next_score = cost(&next, target);
            if next_score < score {
                band = trial;
                response = next;
                score = next_score;
                damping = (damping * 0.3).max(1e-6);
            } else {
                damping *= 8.0;
                if damping > 1e5 {
                    break;
                }
            }
        }
        (band, response)
    }

    fn next_band(&self, residual: &[f64]) -> (Band, Vec<f64>) {
        let mut indices: Vec<_> = (0..residual.len()).collect();
        indices.sort_by(|&left, &right| residual[right].abs().total_cmp(&residual[left].abs()));
        let mut peaks = Vec::new();
        for index in indices {
            if peaks.iter().all(|&other: &usize| other.abs_diff(index) > 20) {
                peaks.push(index);
            }
            if peaks.len() == 4 {
                break;
            }
        }
        let mut seeds = Vec::new();
        for index in peaks {
            for q in [0.35_f64, 0.7, 1.5, 3.0, 6.0, 12.0, 20.0] {
                seeds.push(self.band("PK", [self.frequencies[index].ln(), residual[index] / 12.0, q.ln()]));
            }
        }
        for (kind, edge, direction) in [("LS", 0, 1), ("HS", residual.len() - 1, -1)] {
            if residual[edge].abs() < 0.01 {
                continue;
            }
            let half = residual[edge] * 0.5;
            let crossing = (0..residual.len()).map(|step| edge.wrapping_add_signed(step as isize * direction))
                .find(|&index| residual[index].signum() != half.signum() || residual[index].abs() <= half.abs())
                .unwrap_or(residual.len() / 2);
            for offset in [-64_isize, 0, 64] {
                let index = crossing.saturating_add_signed(offset).min(residual.len() - 1);
                for q in [0.5_f64, 0.707, 1.4] {
                    seeds.push(self.band(kind, [self.frequencies[index].ln(), residual[edge] / 12.0, q.ln()]));
                }
            }
        }
        let mut ranked = seeds.into_iter().map(|band| (cost(&self.response(&band), residual), band)).collect::<Vec<_>>();
        ranked.sort_by(|left, right| left.0.total_cmp(&right.0));
        ranked.into_iter().take(6).map(|(_, band)| self.refine(band, residual, 22))
            .min_by(|a, b| cost(&a.1, residual).total_cmp(&cost(&b.1, residual))).unwrap()
    }

    fn refine_cascade(&self, bands: &mut [Band], target: &[f64], passes: usize) -> Vec<f64> {
        let mut responses: Vec<_> = bands.iter().map(|band| self.response(band)).collect();
        let mut total = vec![0.0; target.len()];
        for response in &responses {
            add_response(&mut total, response, 1.0);
        }
        for _ in 0..passes {
            for (band, response) in bands.iter_mut().zip(&mut responses) {
                add_response(&mut total, response, -1.0);
                let residual = target.iter().zip(&total).map(|(wanted, other)| wanted - other).collect::<Vec<_>>();
                (*band, *response) = self.refine(band.clone(), &residual, 8);
                add_response(&mut total, response, 1.0);
            }
            if max_error(&total, target) <= TOLERANCE_DB {
                break;
            }
        }
        total
    }
}

fn add_response(total: &mut [f64], response: &[f64], scale: f64) {
    for (sample, other) in total.iter_mut().zip(response) {
        *sample += scale * other;
    }
}

fn cost(actual: &[f64], target: &[f64]) -> f64 {
    actual.iter().zip(target).map(|(actual, wanted)| {
        let error2 = (actual - wanted).powi(2);
        error2 * (1.0 + 0.1 * error2)
    }).sum()
}

fn solve_three(mut matrix: [[f64; 4]; 3]) -> [f64; 3] {
    for col in 0..3 {
        let best = (col..3).max_by(|&a, &b| matrix[a][col].abs().total_cmp(&matrix[b][col].abs())).unwrap();
        matrix.swap(col, best);
        let scale = matrix[col][col];
        if scale.abs() < 1e-12 {
            return [0.0; 3];
        }
        for entry in &mut matrix[col][col..] {
            *entry /= scale;
        }
        for row in 0..3 {
            if row != col {
                let factor = matrix[row][col];
                for index in col..4 {
                    matrix[row][index] -= factor * matrix[col][index];
                }
            }
        }
    }
    matrix.map(|row| row[3])
}

fn fit_parametric(frequencies: &[f64], db: &[f64], rate: f64) -> Vec<Band> {
    let fit = ParametricFit {
        frequencies,
        grid: resonance_ipc::fr::ResponseGrid::new(frequencies, rate),
        max_frequency: MAX_FREQUENCY.min(rate * 0.499_99).max(MIN_FREQUENCY),
    };
    let mut bands = Vec::new();
    let mut total = vec![0.0; db.len()];
    let mut best = bands.clone();
    let mut best_error = max_error(&total, db);
    while bands.len() < MAX_BANDS && best_error > TOLERANCE_DB {
        let residual: Vec<_> = db.iter().zip(&total).map(|(target, current)| target - current).collect();
        let (band, response) = fit.next_band(&residual);
        if band.gain_db.abs() < 0.01 || cost(&response, &residual) >= cost(&vec![0.0; db.len()], &residual) * 0.999 {
            break;
        }
        bands.push(band);
        total = fit.refine_cascade(&mut bands, db, 6);
        let error = max_error(&total, db);
        if error < best_error {
            best_error = error;
            best = bands.clone();
        }
    }
    if best_error <= TOLERANCE_DB {
        // Refit after dropping each candidate so small redundant filters do not
        // survive just because the initial search added them in a different order.
        let mut index = 0;
        while index < best.len() {
            let mut reduced = best.clone();
            reduced.remove(index);
            let response = fit.refine_cascade(&mut reduced, db, 6);
            if max_error(&response, db) <= TOLERANCE_DB {
                best = reduced;
                index = 0;
            } else {
                index += 1;
            }
        }
    }
    best
}
