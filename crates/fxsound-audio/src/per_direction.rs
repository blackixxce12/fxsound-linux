//! One value per lane.
//!
//! A lane is one direction's worth of engine state (`docs/0.4.0-design.md` §1.1), and almost
//! everything the engine keeps comes in a pair: the session defaults, the selection memory, the
//! DSP and the paths that feed it. Indexing a pair by [`DeviceDirection`] rather than keeping
//! `output_*` / `input_*` fields side by side is what lets the code that serves one lane be
//! written once and handed the direction, and what makes "the other lane's copy" a thing the
//! compiler can tell apart from "this lane's".
//!
//! Crate-internal: the public API names a lane with a [`DeviceDirection`] argument and never
//! hands out the pair itself.

use fxsound_core::DeviceDirection;

/// One value per [`DeviceDirection`], outputs first.
#[derive(Debug, Default)]
pub(crate) struct PerDirection<T> {
    pub(crate) output: T,
    pub(crate) input: T,
}

impl<T> PerDirection<T> {
    /// Build both values from the direction each is for.
    pub(crate) fn from_fn(mut make: impl FnMut(DeviceDirection) -> T) -> Self {
        Self {
            output: make(DeviceDirection::Output),
            input: make(DeviceDirection::Input),
        }
    }

    pub(crate) const fn get(&self, direction: DeviceDirection) -> &T {
        match direction {
            DeviceDirection::Output => &self.output,
            DeviceDirection::Input => &self.input,
        }
    }

    pub(crate) const fn get_mut(&mut self, direction: DeviceDirection) -> &mut T {
        match direction {
            DeviceDirection::Output => &mut self.output,
            DeviceDirection::Input => &mut self.input,
        }
    }

    /// Both values with the direction each belongs to, outputs first — the order of
    /// [`DeviceDirection::ALL`].
    pub(crate) fn iter(&self) -> impl Iterator<Item = (DeviceDirection, &T)> {
        DeviceDirection::ALL
            .into_iter()
            .zip([&self.output, &self.input])
    }

    /// [`Self::iter`], mutably.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (DeviceDirection, &mut T)> {
        DeviceDirection::ALL
            .into_iter()
            .zip([&mut self.output, &mut self.input])
    }
}

impl<A, B> PerDirection<(A, B)> {
    /// Split a pair of pairs — two channels, two triple buffers — into the pair of ends each side
    /// keeps, so both lanes' paths are created in one expression and cannot be crossed over.
    pub(crate) fn unzip(self) -> (PerDirection<A>, PerDirection<B>) {
        let (output_a, output_b) = self.output;
        let (input_a, input_b) = self.input;
        (
            PerDirection {
                output: output_a,
                input: input_a,
            },
            PerDirection {
                output: output_b,
                input: input_b,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_direction_reaches_its_own_value_and_no_other() {
        let mut pair = PerDirection::from_fn(DeviceDirection::key);
        assert_eq!(*pair.get(DeviceDirection::Output), "output");
        assert_eq!(*pair.get(DeviceDirection::Input), "input");

        *pair.get_mut(DeviceDirection::Input) = "microphone";
        assert_eq!(
            pair.output, "output",
            "writing one lane left the other alone"
        );
        assert_eq!(pair.input, "microphone");

        let seen: Vec<_> = pair.iter().map(|(d, v)| (d, *v)).collect();
        assert_eq!(
            seen,
            [
                (DeviceDirection::Output, "output"),
                (DeviceDirection::Input, "microphone"),
            ],
            "outputs first, like every other per-lane table"
        );
        for (direction, value) in pair.iter_mut() {
            *value = direction.label();
        }
        assert_eq!((pair.output, pair.input), ("Output", "Input"));
    }

    #[test]
    fn unzipping_keeps_each_end_with_its_own_lane() {
        let pairs = PerDirection::from_fn(|d| (d, d.key()));
        let (directions, keys) = pairs.unzip();
        assert_eq!(directions.output, DeviceDirection::Output);
        assert_eq!(directions.input, DeviceDirection::Input);
        assert_eq!((keys.output, keys.input), ("output", "input"));
    }
}
