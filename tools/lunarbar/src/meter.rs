//! What a meter on the bar shows, once it is allowed to remember.
//!
//! A panel that only ever draws the current sample is a panel a printer could
//! have produced: `cpu 52%` does not say whether the machine is climbing out
//! of idle or falling back from a stall. The difference between a picture of
//! a bar and a bar is that the bar kept the last minute and can draw it.
//!
//! Two pieces live here, and both are arithmetic rather than taste:
//!
//! - [`Ring`], one minute of a metric at the panel's 1 Hz tick, squeezed into
//!   however many columns the layout can spare.
//! - [`ramp`], which turns a value into a colour. The palette supplies the
//!   two ends; the sample picks the point between them.

use crate::draw::Rgb;

/// Samples one module keeps. The panel ticks at 1 Hz, so this is one minute
/// of past — long enough that a burst is still on screen when the user looks
/// up from what caused it, short enough to stay inside a bar.
pub const WINDOW: usize = 60;

/// Below this the value is drawn in the calm colour, unshifted. Without a
/// knee every idle machine would sit in a permanent half-warm tint and the
/// colour would stop meaning anything.
pub const KNEE: f32 = 0.55;

/// One metric's recent past, oldest sample first.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Ring {
    v: Vec<f32>,
}

impl Ring {
    /// Record one sample, dropping the oldest once the window is full.
    pub fn push(&mut self, s: f32) {
        if self.v.len() >= WINDOW {
            // One element per tick: the shift is 59 floats once a second,
            // which is cheaper than the indexing a true ring would cost every
            // reader, and it keeps `samples` a plain ordered slice.
            let drop = self.v.len() - WINDOW + 1;
            self.v.drain(..drop);
        }
        self.v.push(s);
    }

    /// Record a percentage reading straight off /proc.
    ///
    /// A tick that failed to read is dropped rather than pushed as a zero: a
    /// metric nobody could sample is not an idle second, and a zero punched
    /// into the window draws a trough the machine never had.
    pub fn push_pct(&mut self, s: Option<u32>) {
        if let Some(v) = s {
            self.push(f32::from(u16::try_from(v).unwrap_or(u16::MAX)) / 100.0);
        }
    }

    /// The window as it stands, oldest first.
    pub fn samples(&self) -> &[f32] {
        &self.v
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// The newest sample — what every other panel would be showing on its own.
    pub fn last(&self) -> Option<f32> {
        self.v.last().copied()
    }

    /// The largest sample in the window. This is the number an auto-scaled
    /// plot is divided by, and the one the bar has to print next to it: a
    /// shape without its scale is decoration.
    pub fn max(&self) -> f32 {
        self.v.iter().copied().fold(0.0, f32::max)
    }

    /// The window squeezed into `cols` columns by taking each bucket's
    /// **maximum**, not its mean.
    ///
    /// A mean would be the obvious choice and the wrong one: a one-second
    /// spike averaged with four quiet neighbours comes out as a bump the
    /// height of a rounding error, and the single event the user is looking
    /// for is the one thing the plot loses. The maximum keeps it.
    ///
    /// A window shorter than `cols` is returned as it is rather than
    /// stretched, so a panel that started twenty seconds ago draws twenty
    /// columns and grows into the rest — a plot that filled the slot from the
    /// first tick would be claiming a past it does not have.
    pub fn pooled(&self, cols: usize) -> Vec<f32> {
        if cols == 0 || self.v.is_empty() {
            return Vec::new();
        }
        if self.v.len() <= cols {
            return self.v.clone();
        }
        let n = self.v.len();
        (0..cols)
            .map(|i| {
                // Bucket boundaries by exact division of the window, so the
                // last bucket always ends on the newest sample.
                let a = i * n / cols;
                let b = ((i + 1) * n / cols).max(a + 1);
                self.v[a..b].iter().copied().fold(f32::MIN, f32::max)
            })
            .collect()
    }

    /// [`Ring::pooled`], divided by the window's own maximum so the shape
    /// fills the slot whatever the units are.
    ///
    /// An all-zero window stays all zeros instead of becoming a division by
    /// nothing, which is the honest answer: an idle minute has no shape.
    pub fn pooled_scaled(&self, cols: usize) -> Vec<f32> {
        let m = self.max();
        let mut out = self.pooled(cols);
        if m > 0.0 {
            for v in &mut out {
                *v /= m;
            }
        } else {
            out.fill(0.0);
        }
        out
    }
}

/// The colour a value has earned, between the palette's calm end and its hot
/// one.
///
/// Below [`KNEE`] this is exactly `calm`, so an idle machine is drawn in the
/// accent and nothing else. Above it the two ends are mixed in a straight
/// line, which means there is no threshold to argue about and no third colour
/// to choose: the sample decides, every frame.
pub fn ramp(v: f32, calm: Rgb, hot: Rgb) -> Rgb {
    let v = v.clamp(0.0, 1.0);
    if v <= KNEE {
        return calm;
    }
    let t = (v - KNEE) / (1.0 - KNEE);
    let mix = |a: u8, b: u8| -> u8 {
        // `round` rather than a truncating cast so t == 1.0 lands on `hot`
        // exactly and the top of the ramp is the colour the palette named.
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
    };
    (mix(calm.0, hot.0), mix(calm.1, hot.1), mix(calm.2, hot.2))
}

/// Draw a module's window in the slot the layout reserved for it.
///
/// The colour is not passed in finished: the newest column picks it off the
/// ramp between `calm` and `hot`, so the same call paints a quiet module in
/// the accent and a pinned one in the warning colour with no branch at the
/// call site.
///
/// `scaled` divides the window by its own maximum, which is what a rate in
/// bytes per second needs and a percentage does not.
#[allow(clippy::too_many_arguments)]
pub fn plot(
    cv: &mut crate::draw::Canvas,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: &Ring,
    scaled: bool,
    back: Rgb,
    calm: Rgb,
    hot: Rgb,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let cols = w.max(0) as usize;
    let d = if scaled {
        r.pooled_scaled(cols)
    } else {
        r.pooled(cols)
    };
    cv.fill_rect_a(x, y, w, h, back, 1.0);
    let c = ramp(d.last().copied().unwrap_or(0.0), calm, hot);
    cv.spark(x, y, w, h, &d, c);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALM: Rgb = (0x3a, 0xd7, 0xe8);
    const HOT: Rgb = (0xff, 0x8a, 0x5c);

    fn ring_of(v: &[f32]) -> Ring {
        let mut r = Ring::default();
        for s in v {
            r.push(*s);
        }
        r
    }

    #[test]
    fn a_ring_keeps_the_newest_minute_and_drops_the_oldest_sample() {
        let r = ring_of(
            &(0..WINDOW as i32 + 10)
                .map(|i| i as f32)
                .collect::<Vec<_>>(),
        );
        assert_eq!(r.len(), WINDOW, "the window must not grow without bound");
        assert_eq!(
            r.samples()[0],
            10.0,
            "the ten oldest samples are the ones that leave"
        );
        assert_eq!(
            r.last(),
            Some(WINDOW as f32 + 9.0),
            "and the newest sample is the one that stays"
        );
    }

    #[test]
    fn a_tick_that_could_not_be_read_leaves_no_trough_in_the_window() {
        let mut r = Ring::default();
        r.push_pct(Some(40));
        r.push_pct(None);
        r.push_pct(Some(42));
        assert_eq!(
            r.samples(),
            &[0.40, 0.42],
            "a failed read must not land in the plot as an idle second"
        );
    }

    #[test]
    fn a_window_squeezed_into_fewer_columns_keeps_its_spike() {
        // One tick at full load in a minute of near-idle: the event the user
        // is looking for.
        let mut v = vec![0.02f32; WINDOW];
        v[30] = 1.0;
        let r = ring_of(&v);
        let cols = 27;
        let p = r.pooled(cols);
        assert_eq!(p.len(), cols);
        assert_eq!(
            p.iter().copied().fold(0.0, f32::max),
            1.0,
            "max-pooling must carry the spike into its column"
        );
        // And this is why it is not a mean: the same bucket averaged.
        let bucket = WINDOW / cols + 1;
        let mean = (1.0 + 0.02 * (bucket - 1) as f32) / bucket as f32;
        assert!(
            mean < 0.4,
            "a mean would have flattened the spike to {mean}, which is the bug this avoids"
        );
    }

    #[test]
    fn a_half_filled_history_draws_what_it_has_instead_of_stretching() {
        let r = ring_of(&[0.1, 0.2, 0.3]);
        let p = r.pooled(27);
        assert_eq!(
            p,
            vec![0.1, 0.2, 0.3],
            "three ticks of uptime is a three-column plot, not a full slot"
        );
    }

    #[test]
    fn the_newest_sample_always_lands_in_the_last_column() {
        // Max-pooling means the right-hand column is the peak of the last
        // two or three seconds, not literally the last sample — which is the
        // point: a spike one tick old must not vanish off the edge. What it
        // may never do is show the newest sample LOWER than it is.
        let mut v: Vec<f32> = (0..WINDOW).map(|i| i as f32 / WINDOW as f32).collect();
        *v.last_mut().unwrap() = 0.42;
        let r = ring_of(&v);
        let p = r.pooled(9);
        assert!(
            *p.last().unwrap() >= 0.42,
            "a plot whose right-hand edge is behind the newest sample is showing a frozen past"
        );
        // When now IS the peak, the last column is exactly it.
        let mut v = vec![0.1f32; WINDOW];
        *v.last_mut().unwrap() = 0.9;
        assert_eq!(*ring_of(&v).pooled(9).last().unwrap(), 0.9);
    }

    #[test]
    fn an_empty_or_zero_window_scales_to_nothing_rather_than_dividing_by_it() {
        assert!(Ring::default().pooled_scaled(27).is_empty());
        let r = ring_of(&[0.0; WINDOW]);
        let p = r.pooled_scaled(27);
        assert_eq!(p.len(), 27);
        assert!(
            p.iter().all(|v| *v == 0.0 && v.is_finite()),
            "an idle minute has no shape, and no NaNs either"
        );
    }

    #[test]
    fn scaling_lifts_the_window_maximum_to_the_top_of_the_slot() {
        let r = ring_of(&[2_000.0, 500.0, 8_000.0, 1_000.0]);
        let p = r.pooled_scaled(4);
        assert_eq!(r.max(), 8_000.0, "the divisor is the window's own peak");
        assert_eq!(p[2], 1.0, "and that peak fills the slot");
        assert_eq!(p[1], 0.0625);
    }

    #[test]
    fn a_calm_value_is_left_alone_and_a_full_one_is_the_hot_colour() {
        assert_eq!(ramp(0.0, CALM, HOT), CALM);
        assert_eq!(ramp(KNEE, CALM, HOT), CALM, "the knee itself is still calm");
        assert_eq!(ramp(1.0, CALM, HOT), HOT);
        assert_eq!(
            ramp(2.0, CALM, HOT),
            HOT,
            "and over-range clamps, not wraps"
        );
    }

    #[test]
    fn the_ramp_moves_one_way_only_between_its_two_ends() {
        let mut prev = ramp(KNEE, CALM, HOT);
        let mut moved = 0;
        for i in 0..=100 {
            let v = KNEE + (1.0 - KNEE) * i as f32 / 100.0;
            let c = ramp(v, CALM, HOT);
            // Red climbs towards HOT, green and blue fall: never the reverse.
            assert!(c.0 >= prev.0 && c.1 <= prev.1 && c.2 <= prev.2, "at {v}");
            if c != prev {
                moved += 1;
            }
            prev = c;
        }
        assert!(
            moved > 20,
            "a ramp that only has {moved} steps is a threshold"
        );
    }
}
