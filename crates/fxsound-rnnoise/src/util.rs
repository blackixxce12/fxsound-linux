//! Utility functions.

const TANSIG_TABLE: [f32; 201] = [
    0.000000, 0.039979, 0.079830, 0.119427, 0.158649, 0.197375, 0.235496, 0.272905, 0.309507,
    0.345214, 0.379949, 0.413644, 0.446244, 0.477700, 0.507977, 0.537050, 0.564900, 0.591519,
    0.616909, 0.641077, 0.664037, 0.685809, 0.706419, 0.725897, 0.744277, 0.761594, 0.777888,
    0.793199, 0.807569, 0.821040, 0.833655, 0.845456, 0.856485, 0.866784, 0.876393, 0.885352,
    0.893698, 0.901468, 0.908698, 0.915420, 0.921669, 0.927473, 0.932862, 0.937863, 0.942503,
    0.946806, 0.950795, 0.954492, 0.957917, 0.961090, 0.964028, 0.966747, 0.969265, 0.971594,
    0.973749, 0.975743, 0.977587, 0.979293, 0.980869, 0.982327, 0.983675, 0.984921, 0.986072,
    0.987136, 0.988119, 0.989027, 0.989867, 0.990642, 0.991359, 0.992020, 0.992631, 0.993196,
    0.993718, 0.994199, 0.994644, 0.995055, 0.995434, 0.995784, 0.996108, 0.996407, 0.996682,
    0.996937, 0.997172, 0.997389, 0.997590, 0.997775, 0.997946, 0.998104, 0.998249, 0.998384,
    0.998508, 0.998623, 0.998728, 0.998826, 0.998916, 0.999000, 0.999076, 0.999147, 0.999213,
    0.999273, 0.999329, 0.999381, 0.999428, 0.999472, 0.999513, 0.999550, 0.999585, 0.999617,
    0.999646, 0.999673, 0.999699, 0.999722, 0.999743, 0.999763, 0.999781, 0.999798, 0.999813,
    0.999828, 0.999841, 0.999853, 0.999865, 0.999875, 0.999885, 0.999893, 0.999902, 0.999909,
    0.999916, 0.999923, 0.999929, 0.999934, 0.999939, 0.999944, 0.999948, 0.999952, 0.999956,
    0.999959, 0.999962, 0.999965, 0.999968, 0.999970, 0.999973, 0.999975, 0.999977, 0.999978,
    0.999980, 0.999982, 0.999983, 0.999984, 0.999986, 0.999987, 0.999988, 0.999989, 0.999990,
    0.999990, 0.999991, 0.999992, 0.999992, 0.999993, 0.999994, 0.999994, 0.999994, 0.999995,
    0.999995, 0.999996, 0.999996, 0.999996, 0.999997, 0.999997, 0.999997, 0.999997, 0.999997,
    0.999998, 0.999998, 0.999998, 0.999998, 0.999998, 0.999998, 0.999999, 0.999999, 0.999999,
    0.999999, 0.999999, 0.999999, 0.999999, 0.999999, 0.999999, 0.999999, 0.999999, 0.999999,
    0.999999, 1.000000, 1.000000, 1.000000, 1.000000, 1.000000, 1.000000, 1.000000, 1.000000,
    1.000000, 1.000000, 1.000000,
];

pub(crate) fn tansig_approx(x: f32) -> f32 {
    // Tests are reversed to catch NaNs
    if !(x < 8.0) {
        return 1.0;
    }
    if !(x > -8.0) {
        return -1.0;
    }

    let (mut x, sign) = if x < 0.0 { (-x, -1.0) } else { (x, 1.0) };
    let i = (0.5 + 25.0 * x).floor();
    x -= 0.04 * i;
    let y = TANSIG_TABLE[i as usize];
    let dy = 1.0 - y * y;
    let y = y + x * dy * (1.0 - y * x);
    sign * y
}

pub(crate) fn sigmoid_approx(x: f32) -> f32 {
    0.5 + 0.5 * tansig_approx(0.5 * x)
}

pub(crate) fn relu(x: f32) -> f32 {
    x.max(0.0)
}

/// Zip 3 things.
pub fn zip3<I, J, K>(i: I, j: J, k: K) -> impl Iterator<Item = (I::Item, J::Item, K::Item)>
where
    I: IntoIterator,
    J: IntoIterator,
    K: IntoIterator,
{
    i.into_iter()
        .zip(j.into_iter().zip(k))
        .map(|(x, (y, z))| (x, y, z))
}

// fxsound: what `Biquad::filter` flushes its state to zero under.
/// A filter state smaller than this is flushed to zero at the end of [`Biquad::filter`]. In the
/// library's 16-bit range it is 690 dB under full scale, so nothing that can be heard is touched;
/// what it stops is the high-pass's decay into a limit cycle on digital silence (see `filter`).
const STATE_FLUSH: f32 = 1.0e-30;

/// A basic high-pass filter.
pub const BIQUAD_HP: Biquad = Biquad {
    a: [-1.99599, 0.99600],
    b: [-2.0, 1.0],
};

/// A biquad filter.
///
/// Our convention here is that both sets of coefficients come with an implicit leading "1". To be
/// precise, if `x` is the input then this filter outputs `y` defined by
/// ```text
/// y[n] = x[n] + b[0] * x[n-1] + b[1] * x[n-2] - a[0] * y[n-1] - a[1] * y[n-2].
/// ```
#[derive(Default)]
pub struct Biquad {
    /// The auto-regressive coefficients.
    pub a: [f32; 2],
    /// The moving-average coefficients.
    pub b: [f32; 2],
}

impl Biquad {
    /// Apply this biquad filter to `input`, putting the result in `output`.
    ///
    /// `mem` is a scratch buffer allowing you filter a long signal one buffer at a time. If you
    /// call this function multiple times with the same `mem` buffer, the output will be as though
    /// you had called it once with a longer `input`. The first time you call `filter` on a given
    /// signal, `mem` should be zero.
    ///
    /// A state that has decayed under `1e-30` is flushed to zero at the end of each call. Upstream
    /// has no such step, and on digital silence — a microphone muted in hardware — its high-pass
    /// never reaches zero: the poles sit at a radius of 0.998, the state is rounded to `f32` after
    /// every step, and once it is subnormal the rounding holds it in a limit cycle around `5e-43`
    /// for good. Every frame after that windowed, transformed and pitch-searched subnormal data
    /// before the silence check skipped the network, per channel, for as long as the microphone
    /// stayed muted. So silence is no longer upstream's bit for bit; audio is.
    pub fn filter(&self, output: &mut [f32], mem: &mut [f32; 2], input: &[f32]) {
        let a0 = self.a[0] as f64;
        let a1 = self.a[1] as f64;
        let b0 = self.b[0] as f64;
        let b1 = self.b[1] as f64;
        for (&x, y) in input.iter().zip(output) {
            let x64 = x as f64;
            let y64 = x64 + mem[0] as f64;
            mem[0] = (mem[1] as f64 + (b0 * x64 - a0 * y64)) as f32;
            mem[1] = (b1 * x64 - a1 * y64) as f32;
            *y = y64 as f32;
        }
        // fxsound: flush a state that has decayed to nothing, before it can become subnormal.
        for state in mem.iter_mut() {
            if state.abs() < STATE_FLUSH {
                *state = 0.0;
            }
        }
    }

    /// Apply this biquad filter to `data`, modifying it in place.
    ///
    /// See [`Biquad::filter`] for more details.
    // This is only used when the "train" feature is active.
    // fxsound: the `train` feature is not vendored, so this is simply unused.
    #[allow(dead_code)]
    pub fn filter_in_place(&self, data: &mut [f32], mem: &mut [f32; 2]) {
        let a0 = self.a[0] as f64;
        let a1 = self.a[1] as f64;
        let b0 = self.b[0] as f64;
        let b1 = self.b[1] as f64;
        for x in data {
            let x64 = *x as f64;
            let y64 = x64 + mem[0] as f64;
            mem[0] = (mem[1] as f64 + (b0 * x64 - a0 * y64)) as f32;
            mem[1] = (b1 * x64 - a1 * y64) as f32;
            *x = y64 as f32;
        }
    }
}

// fxsound: the registry crate had no tests here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_high_pass_comes_to_rest_at_zero_on_digital_silence() {
        // Upstream's filter, with its state rounded to `f32` after every step, decays into the
        // subnormals on a muted microphone and circles there around 5e-43 for good. A second
        // of a loud tone, then three of zeros: the state has to be exactly zero, and so does
        // what the filter hands on, from then on.
        let mut mem = [0.0; 2];
        let tone: Vec<f32> = (0..48_000)
            .map(|n| (n as f32 * std::f32::consts::TAU * 300.0 / 48_000.0).sin() * 16_000.0)
            .collect();
        let mut out = vec![0.0; 480];
        for frame in tone.chunks_exact(480) {
            BIQUAD_HP.filter(&mut out, &mut mem, frame);
        }
        let zeros = [0.0; 480];
        for _ in 0..300 {
            BIQUAD_HP.filter(&mut out, &mut mem, &zeros);
        }
        assert_eq!(mem, [0.0, 0.0]);
        BIQUAD_HP.filter(&mut out, &mut mem, &zeros);
        assert!(out.iter().all(|y| *y == 0.0));
    }

    #[test]
    fn a_signal_is_filtered_exactly_as_upstream_filters_it() {
        // The flush is for a state that has decayed to nothing; anything that can be heard runs
        // upstream's arithmetic unchanged. Upstream's loop, verbatim, as the reference.
        let upstream = |output: &mut [f32], mem: &mut [f32; 2], input: &[f32]| {
            let (a0, a1) = (BIQUAD_HP.a[0] as f64, BIQUAD_HP.a[1] as f64);
            let (b0, b1) = (BIQUAD_HP.b[0] as f64, BIQUAD_HP.b[1] as f64);
            for (&x, y) in input.iter().zip(output) {
                let x64 = x as f64;
                let y64 = x64 + mem[0] as f64;
                mem[0] = (mem[1] as f64 + (b0 * x64 - a0 * y64)) as f32;
                mem[1] = (b1 * x64 - a1 * y64) as f32;
                *y = y64 as f32;
            }
        };
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let hiss: Vec<f32> = (0..48_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 3.0
            })
            .collect();
        let (mut ours, mut theirs) = ([0.0; 2], [0.0; 2]);
        let (mut got, mut want) = (vec![0.0; 480], vec![0.0; 480]);
        for frame in hiss.chunks_exact(480) {
            BIQUAD_HP.filter(&mut got, &mut ours, frame);
            upstream(&mut want, &mut theirs, frame);
            assert_eq!(got, want);
        }
    }
}
