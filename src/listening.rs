use crate::config::Config;

/// Calculate peak and average gain from the active EQ response.
/// Equal steps on a log-frequency axis approximate pink-noise excitation.
fn response(cfg: &Config, sample_rate: f64, channels: usize) -> (f64, f64) {
    if cfg.bypass {
        return (0.0, 0.0);
    }
    let nyquist = sample_rate / 2.0;
    let max_audible = nyquist.min(20000.0);
    let log_ratio = (max_audible / 20.0).max(1.0).ln();
    let audible = (0..=1024).map(|i| 20.0 * (log_ratio * f64::from(i) / 1024.0).exp());
    // Include the endpoints and exact band centers, including narrow peaks.
    let edges = (0..=64).flat_map(|i| {
        let step = f64::from(i) / 64.0;
        [20.0 * step * step, max_audible + (nyquist - max_audible) * step]
    });
    let critical = edges.chain(
        cfg.bands.iter().filter(|band| band.enabled && band.frequency_hz < nyquist)
            .map(|band| band.frequency_hz),
    );
    let points = cfg.response(audible.chain(critical), sample_rate);
    let peak = points.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean_power = points[..1025].iter().map(|db| 10f64.powf(db / 10.0)).sum::<f64>() / 1025.0;
    let (left, right) = cfg.gains();
    let balance_power = if channels >= 2 {
        (left * left + right * right + (channels - 2) as f64) / channels as f64
    } else {
        1.0
    };
    (peak, 10.0 * (mean_power * balance_power).log10())
}

pub fn prepare(
    current: &Config,
    reference: Option<&Config>,
    auto_headroom: bool,
    sample_rate: f64,
    channels: usize,
) -> (Config, Option<Config>) {
    let sample_rate = if sample_rate.is_finite() && sample_rate >= 40.0 { sample_rate } else { 48000.0 };
    let mut current = current.clone();
    if !auto_headroom && reference.is_none() {
        return (current, None);
    }
    let metrics = |cfg: &mut Config| {
        let (peak, average) = response(cfg, sample_rate, channels);
        if auto_headroom && !cfg.bypass && cfg.preamp_db + peak > -0.2 {
            cfg.preamp_db = (-peak - 0.2).max(-60.0);
        }
        (peak, average)
    };
    let (b_peak, b_average) = metrics(&mut current);
    let reference = reference.map(|reference| {
        let mut a = reference.clone();
        // Bypass is a shared listen control for both sides.
        a.bypass = current.bypass;
        let (a_peak, a_average) = metrics(&mut a);
        if !current.bypass {
            // Match by attenuation only and reserve peak headroom on both sides.
            let target = (a.preamp_db + a_average)
                .min(current.preamp_db + b_average)
                .min(a_average - a_peak - 0.2)
                .min(b_average - b_peak - 0.2);
            a.preamp_db = (target - a_average).clamp(-60.0, a.preamp_db);
            current.preamp_db = (target - b_average).clamp(-60.0, current.preamp_db);
        }
        a
    });
    (current, reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_headroom_accounts_for_stacked_filters_and_keeps_manual_cuts() {
        let mut cfg = Config::default();
        for band in cfg.bands.iter_mut().take(2) {
            band.frequency_hz = 1013.0;
            band.gain_db = 6.0;
            band.q = 20.0;
        }
        let (effective, _) = prepare(&cfg, None, true, 48000.0, 2);
        assert!((effective.preamp_db + 12.2).abs() < 0.01);
        cfg.preamp_db = -18.0;
        assert_eq!(prepare(&cfg, None, true, 48000.0, 2).0, cfg);
        cfg.bypass = true;
        assert_eq!(prepare(&cfg, None, true, 48000.0, 2).0, cfg);
    }

    #[test]
    fn comparison_attenuates_the_louder_side_and_leaves_originals_intact() {
        let quiet = Config { preamp_db: -9.0, ..Config::default() };
        let loud = Config { preamp_db: -3.0, ..Config::default() };
        let (b, a) = prepare(&loud, Some(&quiet), false, 48000.0, 2);
        assert!((b.preamp_db + 9.0).abs() < 1e-7);
        assert!((a.unwrap().preamp_db + 9.0).abs() < 1e-7);
        assert_eq!(loud.preamp_db, -3.0);
    }
}
