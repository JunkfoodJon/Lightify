use rand::{Rng, SeedableRng, rngs::SmallRng};
use std::{
    ops::{Deref, DerefMut},
    vec::IntoIter,
};

#[derive(Debug, Clone, Default)]
pub struct ShuffleVec<T> {
    vec: Vec<T>,
    indices: Option<Vec<usize>>,
    /// This is primarily necessary to ensure that shuffle does not behave out of place.
    ///
    /// For that reason we swap the first track with the currently playing track. By that we ensure
    /// that the shuffle state is consistent between resets of the state because the first track is
    /// always the track with which we started playing when switching to shuffle.
    original_first_position: Option<usize>,
    /// Lightify patch: the swaps [`Self::push_shuffled`] made, undone first by
    /// [`Self::unshuffle`]. `indices` only ever describes the list as it was shuffled.
    appended_swaps: Vec<(usize, usize)>,
}

impl<T: PartialEq> PartialEq for ShuffleVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.vec == other.vec
    }
}

impl<T> Deref for ShuffleVec<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.vec
    }
}

impl<T> DerefMut for ShuffleVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.vec.as_mut()
    }
}

impl<T> IntoIterator for ShuffleVec<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.into_iter()
    }
}

impl<T> From<Vec<T>> for ShuffleVec<T> {
    fn from(vec: Vec<T>) -> Self {
        Self {
            vec,
            original_first_position: None,
            indices: None,
            appended_swaps: Vec::new(),
        }
    }
}

impl<T> ShuffleVec<T> {
    pub fn shuffle_with_seed<F: Fn(&T) -> bool>(&mut self, seed: u64, is_first: F) {
        self.shuffle_with_rng(SmallRng::seed_from_u64(seed), is_first)
    }

    pub fn shuffle_with_rng<F: Fn(&T) -> bool>(&mut self, mut rng: impl Rng, is_first: F) {
        if self.indices.is_some() {
            self.unshuffle()
        }

        if self.vec.len() <= 1 {
            info!("skipped shuffling for less or equal one item");
            // Lightify patch: nothing to move yet, but the list is shuffled from here on,
            // so tracks appended later (a chart started with one match) obey it.
            self.indices = Some(Vec::new());
            return;
        }

        let indices: Vec<_> = {
            (1..self.vec.len())
                .rev()
                .map(|i| rng.random_range(0..i + 1))
                .collect()
        };

        for (i, &rnd_ind) in (1..self.vec.len()).rev().zip(&indices) {
            self.vec.swap(i, rnd_ind);
        }

        self.indices = Some(indices);

        self.original_first_position = self.vec.iter().position(is_first);
        if let Some(first_pos) = self.original_first_position {
            self.vec.swap(0, first_pos)
        }
    }

    pub fn unshuffle(&mut self) {
        let indices = match self.indices.take() {
            Some(indices) => indices,
            None => return,
        };

        // Lightify patch: newest first, which leaves the list as it was shuffled plus the
        // appended tracks at the end, in the order they came.
        for (last, at) in self.appended_swaps.drain(..).rev() {
            self.vec.swap(last, at);
        }

        if let Some(first_pos) = self.original_first_position {
            self.vec.swap(0, first_pos);
            self.original_first_position = None;
        }

        // Lightify patch: `indices` covers only the tracks that were there when shuffled.
        // Using the current length here scrambled the list once anything was appended.
        let shuffled_len = (indices.len() + 1).min(self.vec.len());
        for i in 1..shuffled_len {
            match indices.get(shuffled_len - i - 1) {
                None => return,
                Some(n) => self.vec.swap(*n, i),
            }
        }
    }

    /// Lightify patch: add `item` to the end, or while shuffled, at a random place at or
    /// after `from` (where what is already lined up ends). Position 0 stays put: it is the
    /// track the shuffle started on. [`Self::unshuffle`] still restores the original order,
    /// with appended items after it in the order they came.
    pub fn push_shuffled(&mut self, item: T, from: usize) {
        self.push_shuffled_with_rng(item, from, rand::rng())
    }

    /// See [`Self::push_shuffled`].
    pub fn push_shuffled_with_rng(&mut self, item: T, from: usize, mut rng: impl Rng) {
        self.vec.push(item);
        if self.indices.is_none() {
            return;
        }
        let last = self.vec.len() - 1;
        let at = rng.random_range(from.max(1).min(last)..=last);
        if at != last {
            self.vec.swap(last, at);
            self.appended_swaps.push((last, at));
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rand::Rng;
    use std::ops::Range;

    fn base(range: Range<usize>) -> (ShuffleVec<usize>, u64) {
        let seed = rand::rng().random_range(0..10_000_000_000_000);

        let vec = range.collect::<Vec<_>>();
        (vec.into(), seed)
    }

    #[test]
    fn test_shuffle_without_first() {
        let (base_vec, seed) = base(0..100);

        let mut shuffled_vec = base_vec.clone();
        shuffled_vec.shuffle_with_seed(seed, |_| false);

        let mut different_shuffled_vec = base_vec.clone();
        different_shuffled_vec.shuffle_with_seed(seed, |_| false);

        assert_eq!(
            shuffled_vec, different_shuffled_vec,
            "shuffling with the same seed has the same result"
        );

        let mut unshuffled_vec = shuffled_vec.clone();
        unshuffled_vec.unshuffle();

        assert_eq!(
            base_vec, unshuffled_vec,
            "unshuffle restores the original state"
        );
    }

    #[test]
    fn test_shuffle_with_first() {
        const MAX_RANGE: usize = 200;

        let (base_vec, seed) = base(0..MAX_RANGE);
        let rand_first = rand::rng().random_range(0..MAX_RANGE);

        let mut shuffled_with_first = base_vec.clone();
        shuffled_with_first.shuffle_with_seed(seed, |i| i == &rand_first);

        assert_eq!(
            Some(&rand_first),
            shuffled_with_first.first(),
            "after shuffling the first is expected to be the given item"
        );

        let mut shuffled_without_first = base_vec.clone();
        shuffled_without_first.shuffle_with_seed(seed, |_| false);

        let mut switched_positions = Vec::with_capacity(2);
        for (i, without_first_value) in shuffled_without_first.iter().enumerate() {
            if without_first_value != &shuffled_with_first[i] {
                switched_positions.push(i);
            } else {
                assert_eq!(
                    without_first_value, &shuffled_with_first[i],
                    "shuffling with the same seed has the same result"
                );
            }
        }

        assert_eq!(
            switched_positions.len(),
            2,
            "only the switched positions should be different"
        );

        assert_eq!(
            shuffled_with_first[switched_positions[0]],
            shuffled_without_first[switched_positions[1]],
            "the switched values should be equal"
        );

        assert_eq!(
            shuffled_with_first[switched_positions[1]],
            shuffled_without_first[switched_positions[0]],
            "the switched values should be equal"
        )
    }

    // Lightify patch tests.
    #[test]
    fn unshuffle_restores_the_order_after_appends() {
        for first in [None, Some(17usize)] {
            let (mut v, seed) = base(0..40);
            v.shuffle_with_seed(seed, |i| Some(*i) == first);
            let mut rng = SmallRng::seed_from_u64(seed);
            for (n, item) in (40..90).enumerate() {
                v.push_shuffled_with_rng(item, 5 + n / 5, &mut rng);
            }
            v.unshuffle();
            assert_eq!(*v, (0..90).collect::<Vec<_>>(), "first: {first:?}");
        }
    }

    #[test]
    fn appends_land_at_or_after_what_is_lined_up() {
        let (mut v, seed) = base(0..30);
        v.shuffle_with_seed(seed, |_| false);
        let lined_up: Vec<_> = v[..10].to_vec();
        let mut rng = SmallRng::seed_from_u64(seed);
        for item in 30..60 {
            v.push_shuffled_with_rng(item, 10, &mut rng);
        }
        assert_eq!(v[..10], lined_up[..], "what is lined up doesn't move");
        let (mut sorted, want) = (v.to_vec(), (0..60).collect::<Vec<_>>());
        sorted.sort();
        assert_eq!(sorted, want, "nothing lost or doubled");
    }

    #[test]
    fn a_list_started_with_one_track_obeys_shuffle_when_it_grows() {
        let mut v: ShuffleVec<usize> = vec![0].into();
        v.shuffle_with_seed(7, |i| *i == 0);
        let mut rng = SmallRng::seed_from_u64(7);
        for item in 1..50 {
            v.push_shuffled_with_rng(item, 1, &mut rng);
        }
        assert_eq!(v[0], 0, "the track playing stays first");
        assert_ne!(*v, (0..50).collect::<Vec<_>>(), "shuffled, not in chart order");
        v.unshuffle();
        assert_eq!(*v, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn appends_while_not_shuffled_stay_in_order() {
        let mut v: ShuffleVec<usize> = vec![0, 1].into();
        v.push_shuffled(2, 1);
        v.push_shuffled(3, 1);
        assert_eq!(*v, vec![0, 1, 2, 3]);
    }

    #[test]
    fn reshuffling_after_appends_covers_the_whole_list() {
        let (mut v, seed) = base(0..10);
        v.shuffle_with_seed(seed, |_| false);
        let mut rng = SmallRng::seed_from_u64(seed);
        for item in 10..20 {
            v.push_shuffled_with_rng(item, 3, &mut rng);
        }
        v.shuffle_with_seed(seed + 1, |_| false);
        v.unshuffle();
        assert_eq!(*v, (0..20).collect::<Vec<_>>());
    }
}
