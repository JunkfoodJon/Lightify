use crate::{
    core::{Error, SpotifyUri},
    protocol::player::ProvidedTrack,
    state::{
        ConnectState, SPOTIFY_MAX_NEXT_TRACKS_SIZE, SPOTIFY_MAX_PREV_TRACKS_SIZE, StateError,
        context::ContextType,
        metadata::Metadata,
        provider::{IsProvider, Provider},
    },
};
use protobuf::MessageField;
use rand::Rng;

// identifier used as part of the uid
pub const IDENTIFIER_DELIMITER: &str = "delimiter";

impl<'ct> ConnectState {
    fn new_delimiter(iteration: i64) -> ProvidedTrack {
        let mut delimiter = ProvidedTrack {
            uri: format!("spotify:{IDENTIFIER_DELIMITER}"),
            uid: format!("{IDENTIFIER_DELIMITER}{iteration}"),
            provider: Provider::Context.to_string(),
            ..Default::default()
        };
        delimiter.set_hidden(true);
        delimiter.set_iteration(iteration);

        delimiter
    }

    fn push_prev(&mut self, prev: ProvidedTrack) {
        let prev_tracks = self.prev_tracks_mut();
        // add prev track, while preserving a length of 10
        if prev_tracks.len() >= SPOTIFY_MAX_PREV_TRACKS_SIZE {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_PREV_TRACKS_SIZE) aka O(10)
            let _ = prev_tracks.remove(0);
        }
        prev_tracks.push(prev)
    }

    fn get_next_track(&mut self) -> Option<ProvidedTrack> {
        if self.next_tracks().is_empty() {
            None
        } else {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_NEXT_TRACKS_SIZE) aka O(80)
            Some(self.next_tracks_mut().remove(0))
        }
    }

    /// bottom => top, aka the last track of the list is the prev track
    fn prev_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().prev_tracks
    }

    /// bottom => top, aka the last track of the list is the prev track
    pub(super) fn prev_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().prev_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    fn next_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().next_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    /// Lightify patch: is `uri` in up next (for `Spirc::skip_to`).
    pub fn has_up_next(&self, uri: &str) -> bool {
        self.next_tracks().iter().any(|t| t.uri == uri)
    }

    pub(super) fn next_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().next_tracks
    }

    pub fn set_current_track_random(&mut self) -> Result<(), Error> {
        let max_tracks = self.get_context(self.active_context)?.tracks.len();
        let rng_track = rand::rng().random_range(0..max_tracks);
        self.set_current_track(rng_track)
    }

    pub fn set_current_track(&mut self, index: usize) -> Result<(), Error> {
        let context = self.get_context(self.active_context)?;

        let new_track = context
            .tracks
            .get(index)
            .ok_or(StateError::CanNotFindTrackInContext(
                Some(index),
                context.tracks.len(),
            ))?;

        debug!(
            "set track to: {} at {} of {} tracks",
            new_track.uri,
            index,
            context.tracks.len()
        );

        self.set_track(new_track.clone());

        self.update_current_index(|i| i.track = index as u32);

        Ok(())
    }

    /// Move to the next track
    ///
    /// Updates the current track to the next track. Adds the old track
    /// to prev tracks and fills up the next tracks from the current context
    pub fn next_track(&mut self) -> Result<Option<u32>, Error> {
        // when we skip in repeat track, we don't repeat the current track anymore
        if self.repeat_track() {
            self.set_repeat_track(false);
        }

        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            // only add songs from our context to our previous tracks
            if old_track.is_context() || old_track.is_autoplay() {
                self.push_prev(old_track)
            }
        }

        let new_track = loop {
            match self.get_next_track() {
                Some(next) if next.uid.starts_with(IDENTIFIER_DELIMITER) => {
                    self.push_prev(next);
                    continue;
                }
                Some(next) if next.is_unavailable() => continue,
                other => break other,
            };
        };

        let new_track = match new_track {
            None => return Ok(None),
            Some(t) => t,
        };

        // Lightify patch (BUGS-PLAN DA4): no context to refill from (a load that failed)
        // isn't an error here: the song taken plays, and the list ends after what is left,
        // rather than Next and the song's end failing with every press.
        if let Err(why) = self.fill_up_next_tracks() {
            warn!("next track: nothing to refill the list from ({why}); it ends after what is left");
        }

        let update_index = if new_track.is_queue() {
            None
        } else if new_track.is_autoplay() {
            self.set_active_context(ContextType::Autoplay);
            None
        } else {
            match new_track.get_context_index() {
                Some(new_index) => Some(new_index as u32),
                None => {
                    error!("the given context track had no set context_index");
                    None
                }
            }
        };

        if let Some(update_index) = update_index {
            self.update_current_index(|i| i.track = update_index)
        } else {
            self.player_mut().index.clear()
        }

        self.set_track(new_track);
        self.update_restrictions();

        Ok(Some(self.player().index.track))
    }

    /// Move to the prev track
    ///
    /// Updates the current track to the prev track. Adds the old track
    /// to next tracks (when from the context) and fills up the prev tracks from the
    /// current context
    ///
    /// Lightify patch: with nothing to go back to, returns `None` having changed nothing.
    pub fn prev_track(&mut self) -> Result<Option<&MessageField<ProvidedTrack>>, Error> {
        if !self.has_prev_track() {
            return Ok(None);
        }

        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            if old_track.is_context() || old_track.is_autoplay() {
                // todo: O(n)
                self.next_tracks_mut().insert(0, old_track);
            }
        }

        // handle possible delimiter
        if matches!(self.prev_tracks().last(), Some(prev) if prev.uid.starts_with(IDENTIFIER_DELIMITER))
        {
            let delimiter = self
                .prev_tracks_mut()
                .pop()
                .expect("item that was prechecked");

            // todo: O(n)
            self.next_tracks_mut().insert(0, delimiter)
        }

        self.trim_next_tracks();

        let new_track = match self.prev_tracks_mut().pop() {
            None => return Ok(None),
            Some(t) => t,
        };

        if matches!(self.active_context, ContextType::Autoplay if new_track.is_context()) {
            // transition back to default context
            self.set_active_context(ContextType::Default);
        }

        self.fill_up_next_tracks()?;
        self.set_track(new_track);

        if self.player().index.track == 0 {
            warn!("prev: trying to skip into negative, index update skipped")
        } else {
            self.update_current_index(|i| i.track -= 1)
        }

        self.update_restrictions();

        Ok(Some(self.current_track(|t| t)))
    }

    pub fn current_track<F: Fn(&'ct MessageField<ProvidedTrack>) -> R, R>(
        &'ct self,
        access: F,
    ) -> R {
        access(&self.player().track)
    }

    pub fn set_track(&mut self, track: ProvidedTrack) {
        self.player_mut().track = MessageField::some(track)
    }

    pub fn set_next_tracks(&mut self, mut tracks: Vec<ProvidedTrack>) {
        // mobile only sends a set_queue command instead of an add_to_queue command
        // in addition to handling the mobile add_to_queue handling, this should also handle
        // a mass queue addition
        tracks
            .iter_mut()
            .filter(|t| t.is_from_queue())
            .for_each(|t| {
                t.set_provider(Provider::Queue);
                // technically we could preserve the queue-uid here,
                // but it seems to work without that, so we just override it
                t.uid = format!("q{}", self.queue_count);
                self.queue_count += 1;
            });

        // when you drag 'n drop the current track in the queue view into the "Next from: ..."
        // section, it is only send as an empty item with just the provider and metadata, so we have
        // to provide set the uri from the current track manually
        tracks
            .iter_mut()
            .filter(|t| t.uri.is_empty())
            .for_each(|t| t.uri = self.current_track(|ct| ct.uri.clone()));

        self.player_mut().next_tracks = tracks;
    }

    pub fn set_prev_tracks(&mut self, tracks: Vec<ProvidedTrack>) {
        self.player_mut().prev_tracks = tracks;
    }

    pub fn clear_prev_track(&mut self) {
        self.prev_tracks_mut().clear()
    }

    pub fn clear_next_tracks(&mut self) {
        // respect queued track and don't throw them out of our next played tracks
        // Lightify patch: wherever they are. `prev_track` puts a context track ahead of
        // them, and cutting at the first non-queued track then threw them all away.
        self.next_tracks_mut().retain(|track| track.is_queue());
    }

    /// Lightify patch: is there a track to go back to? `prev_tracks` may also hold
    /// delimiters, and is capped, so this can be false well into a list.
    fn has_prev_track(&self) -> bool {
        self.prev_tracks()
            .iter()
            .any(|t| !t.uid.starts_with(IDENTIFIER_DELIMITER))
    }

    /// Lightify patch: the first context track not removed by hand.
    pub(super) fn first_kept_index(&self) -> usize {
        self.get_context(self.active_context)
            .ok()
            .and_then(|ctx| {
                ctx.tracks
                    .iter()
                    .position(|t| !self.removed_uids.contains(&t.uid))
            })
            .unwrap_or_default()
    }

    /// Lightify patch: see `removed_uids`.
    pub fn forget_removed_tracks(&mut self) {
        self.removed_uids.clear();
    }

    /// Lightify patch: hold `next_tracks` to Spotify's limit without losing anything. What
    /// falls off the end was already taken from its source, so it goes back there: a
    /// context track by moving the context's index back to it (upstream left the index
    /// past it, so the track was never seen again), a queued one to `queue_overflow`.
    fn trim_next_tracks(&mut self) {
        while self.next_tracks().len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let Some(last) = self.next_tracks_mut().pop() else {
                break;
            };
            if last.is_queue() {
                self.queue_overflow.push_front(last);
            } else {
                self.rewind_context_to(&last);
            }
        }
    }

    /// Lightify patch: `popped` was the last track taken from its context, so filling up
    /// has to take it again. Found by uid, not by stepping back one: skipped tracks can
    /// sit between it and the index.
    fn rewind_context_to(&mut self, popped: &ProvidedTrack) {
        if popped.uid.starts_with(IDENTIFIER_DELIMITER) {
            // Back to the end of the pass it closed; filling up adds it again. One that led
            // into autoplay goes back to filling from the context it closed.
            self.fill_up_context = ContextType::Default;
            if let Ok(ctx) = self.get_context_mut(ContextType::Default) {
                ctx.index.track = ctx.tracks.len() as u32;
            }
            return;
        }

        let ty = if popped.is_autoplay() {
            ContextType::Autoplay
        } else {
            ContextType::Default
        };
        let Ok(ctx) = self.get_context_mut(ty) else {
            return;
        };
        let taken = (ctx.index.track as usize).min(ctx.tracks.len());
        match ctx.tracks[..taken].iter().rposition(|t| t.uid == popped.uid) {
            Some(position) => ctx.index.track = position as u32,
            None => debug!("couldn't find {} in its context to rewind to", popped.uri),
        }
    }

    /// Lightify patch: move queued tracks from `queue_overflow` into `next_tracks`, behind
    /// the tracks queued before them, for as long as Spotify's limit allows.
    fn promote_queue_overflow(&mut self) {
        while !self.queue_overflow.is_empty()
            && self.next_tracks().iter().filter(|t| t.is_queue()).count()
                < SPOTIFY_MAX_NEXT_TRACKS_SIZE
        {
            let Some(track) = self.queue_overflow.pop_front() else {
                break;
            };
            let next_tracks = self.next_tracks_mut();
            let at = next_tracks
                .iter()
                .rposition(|t| t.is_queue())
                .map_or(0, |last_queued| last_queued + 1);
            next_tracks.insert(at, track);
        }
        self.trim_next_tracks();
    }

    /// Lightify patch: up-next after songs were appended to the playing list (a chart, a
    /// station; `fill_context_from_page`). Normally that only tops the window up. With
    /// repeat on, though, the window was already full of the list going round again
    /// (delimiter, its first song, …), so the new songs would only come after dozens of
    /// repeats: the window is rebuilt from the song playing instead, songs queued by hand
    /// kept (BUGS-PLAN QD1).
    pub fn fill_up_after_append(&mut self) -> Result<(), Error> {
        let wrapped = self.next_tracks().iter().any(|t| t.uid.starts_with(IDENTIFIER_DELIMITER));
        if self.repeat_context() && wrapped && matches!(self.fill_up_context, ContextType::Default) {
            let at = self.player().index.track as usize;
            self.clear_next_tracks();
            self.update_context_index(ContextType::Default, at + 1)?;
        }
        self.fill_up_next_tracks()
    }

    /// Lightify patch: the list had ended (Spirc stops at the end and goes back to the
    /// list's start) when more songs were appended: the first appended one becomes the
    /// current song and the rest follow it. Stepping to the next track went to the list's
    /// second song instead (BUGS-PLAN QD3). False if it isn't in the list.
    pub fn go_to_appended(&mut self, first_uri: &str) -> Result<bool, Error> {
        let found = Self::find_index_in_context(self.get_context(ContextType::Default)?, |t| t.uri == first_uri);
        let Ok(at) = found else { return Ok(false) };
        self.reset_playback_to_position(Some(at))?;
        Ok(true)
    }

    pub fn fill_up_next_tracks(&mut self) -> Result<(), Error> {
        self.promote_queue_overflow();

        let ctx = self.get_context(self.fill_up_context)?;
        let mut new_index = ctx.index.track as usize;
        let mut iteration = ctx.index.page;

        while self.next_tracks().len() < SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let ctx = self.get_context(self.fill_up_context)?;
            let track = match ctx.tracks.get(new_index) {
                None if self.repeat_context() => {
                    let delimiter = Self::new_delimiter(iteration.into());
                    iteration += 1;
                    new_index = 0;
                    delimiter
                }
                None if !matches!(self.fill_up_context, ContextType::Autoplay)
                    && self.autoplay_context.is_some()
                    && !self.repeat_context() =>
                {
                    self.update_context_index(self.fill_up_context, new_index)?;

                    // transition to autoplay as fill up context
                    self.fill_up_context = ContextType::Autoplay;
                    new_index = self.get_context(ContextType::Autoplay)?.index.track as usize;

                    // add delimiter to only display the current context
                    Self::new_delimiter(iteration.into())
                }
                None if self.autoplay_context.is_some() => {
                    match self
                        .get_context(ContextType::Autoplay)?
                        .tracks
                        .get(new_index)
                    {
                        None => break,
                        Some(ct) => {
                            new_index += 1;
                            ct.clone()
                        }
                    }
                }
                None => break,
                Some(ct)
                    if ct.is_unavailable()
                        || self.is_skip_track(ct, Some(iteration))
                        || self.removed_uids.contains(&ct.uid) =>
                {
                    debug!(
                        "skipped track {} during fillup as it's unavailable or should be skipped",
                        ct.uri
                    );
                    new_index += 1;
                    continue;
                }
                Some(ct) => {
                    new_index += 1;
                    ct.clone()
                }
            };

            self.next_tracks_mut().push(track);
        }

        debug!(
            "finished filling up next_tracks ({})",
            self.next_tracks().len()
        );

        self.update_context_index(self.fill_up_context, new_index)?;

        // the web-player needs a revision update, otherwise the queue isn't updated in the ui
        self.update_queue_revision();

        Ok(())
    }

    pub fn preview_next_track(&mut self) -> Option<SpotifyUri> {
        let next = if self.repeat_track() {
            self.current_track(|t| &t.uri)
        } else {
            &self.next_tracks().first()?.uri
        };

        SpotifyUri::from_uri(next).ok()
    }

    pub fn has_next_tracks(&self, min: Option<usize>) -> bool {
        if let Some(min) = min {
            self.next_tracks().len() >= min
        } else {
            !self.next_tracks().is_empty()
        }
    }

    pub fn recent_track_uris(&self) -> Vec<String> {
        let mut prev = self
            .prev_tracks()
            .iter()
            .map(|t| t.uri.clone())
            .collect::<Vec<_>>();

        prev.push(self.current_track(|t| t.uri.clone()));
        prev
    }

    pub fn mark_unavailable(&mut self, id: &SpotifyUri) -> Result<(), Error> {
        let uri = id.to_uri()?;

        debug!("marking {uri} as unavailable");

        let next_tracks = self.next_tracks_mut();
        while let Some(pos) = next_tracks.iter().position(|t| t.uri == uri) {
            let _ = next_tracks.remove(pos);
        }

        for next_track in next_tracks {
            Self::mark_as_unavailable_for_match(next_track, &uri)
        }

        let prev_tracks = self.prev_tracks_mut();
        while let Some(pos) = prev_tracks.iter().position(|t| t.uri == uri) {
            let _ = prev_tracks.remove(pos);
        }

        for prev_track in prev_tracks {
            Self::mark_as_unavailable_for_match(prev_track, &uri)
        }

        self.queue_overflow.retain(|t| t.uri != uri);

        self.unavailable_uri.push(uri);
        self.fill_up_next_tracks()?;
        self.update_queue_revision();

        Ok(())
    }

    /// Lightify patch: drop the `nth` (0-based) entry for `uri` from what plays next,
    /// leaving everything around it as it was: queued tracks (in `next_tracks` or waiting
    /// for room there), then the rest of `next_tracks`, then the context past it, which is
    /// the order they play in. A context track also stays gone when the context is gone
    /// through again (see `removed_uids`). Delimiters are never matched. Returns whether
    /// something was removed.
    pub fn remove_next_track(&mut self, uri: &str, nth: usize) -> bool {
        let removed = self.take_next_track(uri, nth);
        if removed {
            self.update_restrictions();
        }
        removed
    }

    fn take_next_track(&mut self, uri: &str, nth: usize) -> bool {
        // Where the `nth` match is in `tracks`, counting down `left` past those it isn't.
        fn find<'t>(
            tracks: impl Iterator<Item = &'t ProvidedTrack>,
            uri: &str,
            left: &mut usize,
        ) -> Option<usize> {
            for (i, track) in tracks.enumerate() {
                if track.uri != uri {
                    continue;
                }
                if *left == 0 {
                    return Some(i);
                }
                *left -= 1;
            }
            None
        }

        let mut left = nth;
        let next_tracks = self.next_tracks();
        let queued = next_tracks
            .iter()
            .position(|t| !t.is_queue())
            .unwrap_or(next_tracks.len());

        if let Some(i) = find(next_tracks[..queued].iter(), uri, &mut left) {
            self.next_tracks_mut().remove(i);
            return true;
        }

        if let Some(i) = find(self.queue_overflow.iter(), uri, &mut left) {
            self.queue_overflow.remove(i);
            return true;
        }

        if let Some(i) = find(self.next_tracks()[queued..].iter(), uri, &mut left) {
            let track = self.next_tracks_mut().remove(queued + i);
            if !track.is_queue() {
                self.removed_uids.insert(track.uid);
            }
            return true;
        }

        let Ok(ctx) = self.get_context(self.fill_up_context) else {
            return false;
        };
        let taken = (ctx.index.track as usize).min(ctx.tracks.len());
        let waiting = ctx.tracks[taken..]
            .iter()
            .filter(|t| !t.is_unavailable() && !self.removed_uids.contains(&t.uid));
        let Some(i) = find(waiting.clone(), uri, &mut left) else {
            return false;
        };
        let Some(uid) = waiting.clone().nth(i).map(|t| t.uid.clone()) else {
            return false;
        };
        self.removed_uids.insert(uid);
        true
    }

    pub fn add_to_queue(&mut self, mut track: ProvidedTrack, rev_update: bool) {
        track.uid = format!("q{}", self.queue_count);
        self.queue_count += 1;

        track.set_provider(Provider::Queue);
        if !track.is_from_queue() {
            track.set_from_queue(true);
        }

        // Lightify patch: behind any queued tracks still waiting for room, and over the
        // limit nothing is lost: see `trim_next_tracks`.
        if !self.queue_overflow.is_empty() {
            self.queue_overflow.push_back(track);
        } else {
            // Lightify patch (BUGS-PLAN DA7): right after the last queued track, as
            // `promote_queue_overflow` does. Prev puts the track left behind at the front
            // (a context track), and inserting at the first non-queued place then put a new
            // song ahead of every song queued before it.
            let next_tracks = self.next_tracks_mut();
            let at = next_tracks.iter().rposition(|t| t.is_queue()).map_or(0, |last| last + 1);
            next_tracks.insert(at, track);

            self.trim_next_tracks();
        }

        if rev_update {
            self.update_queue_revision();
        }
        self.update_restrictions();
    }
}

// Lightify patch tests: the window arithmetic of `next_tracks`, without a session.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        connect::{Device, DeviceInfo, PutStateRequest},
        player::{ContextIndex, PlayerState},
    };
    use crate::state::context::StateContext;
    use std::collections::HashMap;

    const MAX: usize = SPOTIFY_MAX_NEXT_TRACKS_SIZE;

    fn track(name: String, provider: Provider, index: Option<usize>) -> ProvidedTrack {
        let mut t = ProvidedTrack {
            uid: format!("uid-{name}-{index:?}"),
            uri: name,
            provider: provider.to_string(),
            ..Default::default()
        };
        if let Some(index) = index {
            t.set_context_index(index);
        }
        t
    }

    fn context(names: impl Iterator<Item = String>, provider: Provider) -> StateContext {
        let tracks: Vec<_> = names
            .enumerate()
            .map(|(i, name)| track(name, provider.clone(), Some(i)))
            .collect();
        StateContext {
            tracks: tracks.into(),
            metadata: HashMap::new(),
            restrictions: None,
            index: ContextIndex::new(),
        }
    }

    fn names(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("t{i}")).collect()
    }

    /// Playing track 0 of a context of `len` tracks, with the window filled.
    fn state(len: usize) -> ConnectState {
        let mut s = ConnectState {
            request: PutStateRequest {
                device: MessageField::some(Device {
                    device_info: MessageField::some(DeviceInfo::default()),
                    player_state: MessageField::some(PlayerState::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        s.reset();
        s.context = Some(context(names(0..len).into_iter(), Provider::Context));
        s.reset_playback_to_position(Some(0)).unwrap();
        s
    }

    fn queue(s: &mut ConnectState, name: impl Into<String>) {
        let track = ProvidedTrack {
            uri: name.into(),
            ..Default::default()
        };
        s.add_to_queue(track, false);
    }

    fn current(s: &ConnectState) -> String {
        s.player().track.uri.clone()
    }

    /// Skips forward through up to `at_most` tracks, checking the limit on the way.
    fn play_out(s: &mut ConnectState, at_most: usize) -> Vec<String> {
        let mut played = Vec::new();
        while played.len() < at_most {
            assert!(s.next_tracks().len() <= MAX, "over Spotify's limit");
            if s.next_track().unwrap().is_none() {
                break;
            }
            played.push(current(s));
        }
        played
    }

    fn upcoming(s: &ConnectState) -> Vec<String> {
        s.next_tracks().iter().map(|t| t.uri.clone()).collect()
    }

    #[test]
    fn queueing_over_a_full_window_skips_no_context_track() {
        let mut s = state(200);
        assert_eq!(s.next_tracks().len(), MAX);
        queue(&mut s, "q0");
        assert_eq!(s.next_tracks().len(), MAX);
        assert_eq!(s.get_context(ContextType::Default).unwrap().index.track, 80);

        let mut want = vec!["q0".to_string()];
        want.extend(names(1..200));
        assert_eq!(play_out(&mut s, 1000), want);
    }

    #[test]
    fn rewinding_finds_the_track_past_skipped_ones() {
        let mut s = state(120);
        // Tracks filling up has passed over, right where a step back by one would land.
        let ctx = s.get_context_mut(ContextType::Default).unwrap();
        for i in [79, 78, 76] {
            ctx.tracks.get_mut(i).unwrap().set_provider(Provider::Unavailable);
        }
        s.clear_next_tracks();
        s.update_context_index(ContextType::Default, 1).unwrap();
        s.fill_up_next_tracks().unwrap();
        for n in 0..6 {
            queue(&mut s, format!("q{n}"));
        }

        let played = play_out(&mut s, 1000);
        let mut want: Vec<_> = (0..6).map(|n| format!("q{n}")).collect();
        want.extend((1..120).filter(|i| ![79, 78, 76].contains(i)).map(|i| format!("t{i}")));
        assert_eq!(played, want);
    }

    /// DA7: a song queued after Prev goes behind the songs queued before it, not ahead.
    #[test]
    fn a_song_queued_after_prev_waits_behind_the_queue() {
        let mut s = state(20);
        play_out(&mut s, 3);
        queue(&mut s, "qa");
        queue(&mut s, "qb");
        assert!(s.prev_track().unwrap().is_some());
        queue(&mut s, "qc");
        let next = upcoming(&s);
        let at = |n: &str| next.iter().position(|u| u == n).unwrap();
        assert!(at("qa") < at("qb") && at("qb") < at("qc"), "{next:?}");
    }

    #[test]
    fn going_back_with_a_full_window_loses_nothing() {
        let mut s = state(200);
        play_out(&mut s, 5);
        assert_eq!(current(&s), "t5");
        for back in ["t4", "t3", "t2"] {
            assert!(s.prev_track().unwrap().is_some());
            assert_eq!(current(&s), back);
            assert_eq!(s.next_tracks().len(), MAX);
        }
        assert_eq!(play_out(&mut s, 1000), names(3..200));
    }

    #[test]
    fn more_queued_than_the_limit_are_all_played_in_order() {
        let mut s = state(100);
        for n in 0..250 {
            queue(&mut s, format!("q{n}"));
        }
        assert_eq!(s.next_tracks().len(), MAX);
        assert_eq!(s.queue_overflow.len(), 170);
        assert!(s.next_tracks().iter().all(|t| t.is_queue()));

        let mut want: Vec<_> = (0..250).map(|n| format!("q{n}")).collect();
        want.extend(names(1..100));
        assert_eq!(play_out(&mut s, 1000), want);
        assert!(s.queue_overflow.is_empty());
    }

    #[test]
    fn a_track_queued_during_overflow_waits_behind_the_earlier_ones() {
        let mut s = state(10);
        for n in 0..85 {
            queue(&mut s, format!("q{n}"));
        }
        play_out(&mut s, 2);
        assert_eq!(s.queue_overflow.len(), 3);
        queue(&mut s, "late");
        let played = play_out(&mut s, 200);
        assert_eq!(played[..83].last().map(String::as_str), Some("q84"));
        assert_eq!(played[83], "late");
        assert_eq!(played[84], "t1");
    }

    #[test]
    fn a_reset_empties_the_overflow_too() {
        let mut s = state(10);
        for n in 0..85 {
            queue(&mut s, format!("q{n}"));
        }
        s.reset();
        assert!(s.queue_overflow.is_empty());
    }

    #[test]
    fn repeat_keeps_every_track_when_the_window_wrapped() {
        let mut s = state(30);
        s.set_repeat_context(true);
        s.reset_playback_to_position(Some(0)).unwrap();
        assert!(upcoming(&s).iter().any(|u| u == "spotify:delimiter"));
        // Enough to push out wrapped tracks, a delimiter and the first pass's tail.
        for n in 0..60 {
            queue(&mut s, format!("q{n}"));
        }

        let played = play_out(&mut s, 60 + 29 + 30 + 30);
        let mut want: Vec<_> = (0..60).map(|n| format!("q{n}")).collect();
        want.extend(names(1..30));
        want.extend(names(0..30));
        want.extend(names(0..30));
        assert_eq!(played, want);
    }

    #[test]
    fn autoplay_tail_survives_being_pushed_out() {
        let mut s = state(5);
        s.autoplay_context = Some(context(
            (0..200).map(|i| format!("a{i}")),
            Provider::Autoplay,
        ));
        s.fill_up_next_tracks().unwrap();
        assert_eq!(upcoming(&s)[..6], ["t1", "t2", "t3", "t4", "spotify:delimiter", "a0"]);
        // Past the autoplay tracks, the delimiter and into the context's own.
        for n in 0..78 {
            queue(&mut s, format!("q{n}"));
        }
        assert_eq!(upcoming(&s)[78..], ["t1", "t2"]);

        let played = play_out(&mut s, 78 + 4 + 200);
        let mut want: Vec<_> = (0..78).map(|n| format!("q{n}")).collect();
        want.extend(names(1..5));
        want.extend((0..200).map(|i| format!("a{i}")));
        assert_eq!(played, want);
    }

    #[test]
    fn prev_on_the_first_track_changes_nothing() {
        let mut s = state(100);
        queue(&mut s, "q0");
        let before = upcoming(&s);
        assert!(s.prev_track().unwrap().is_none());
        assert_eq!(current(&s), "t0");
        assert_eq!(upcoming(&s), before);
    }

    #[test]
    fn prev_past_the_remembered_tracks_stays_on_the_track() {
        let mut s = state(100);
        play_out(&mut s, 30);
        for _ in 0..SPOTIFY_MAX_PREV_TRACKS_SIZE {
            assert!(s.prev_track().unwrap().is_some());
        }
        assert_eq!(current(&s), "t20");
        assert!(s.prev_track().unwrap().is_none());
        assert_eq!(current(&s), "t20", "upstream jumped to the first track and stopped");
        assert_eq!(play_out(&mut s, 2), ["t21", "t22"]);
    }

    /// What `handle_stop` does to the state; the rest of it needs a player.
    fn stop(s: &mut ConnectState) {
        s.clear_next_tracks();
        s.reset_playback_to_position(None).unwrap();
    }

    #[test]
    fn a_finished_list_leaves_its_first_track_to_play_again() {
        let mut s = state(3);
        assert_eq!(play_out(&mut s, 10), ["t1", "t2"]);
        assert!(s.player().track.is_none());
        stop(&mut s);
        // What `handle_play` starts from `Stopped`.
        assert_eq!(current(&s), "t0");
        assert_eq!(upcoming(&s), ["t1", "t2"]);
        assert_eq!(play_out(&mut s, 10), ["t1", "t2"]);
    }

    #[test]
    fn clearing_spares_queued_tracks_behind_a_context_track() {
        let mut s = state(20);
        play_out(&mut s, 3);
        queue(&mut s, "q0");
        queue(&mut s, "q1");
        assert!(s.prev_track().unwrap().is_some());
        assert_eq!(upcoming(&s)[..3], ["t3", "q0", "q1"]);
        s.clear_next_tracks();
        assert_eq!(upcoming(&s), ["q0", "q1"]);
    }

    #[test]
    fn a_removed_track_in_the_window_stays_gone_on_repeat() {
        let mut s = state(10);
        assert!(s.remove_next_track("t4", 0));
        s.fill_up_next_tracks().unwrap();
        s.set_repeat_context(true);
        s.reset_playback_to_position(Some(0)).unwrap();
        let played = play_out(&mut s, 40);
        assert_eq!(played.len(), 40);
        assert!(played.iter().all(|t| t != "t4"), "{played:?}");
        assert!(played.iter().filter(|t| *t == "t5").count() >= 4);
    }

    #[test]
    fn a_track_past_the_window_can_be_removed() {
        let mut s = state(200);
        let before = upcoming(&s);
        assert!(s.remove_next_track("t150", 0));
        assert_eq!(upcoming(&s), before, "the window isn't touched");
        let played = play_out(&mut s, 1000);
        assert_eq!(played.len(), 198);
        assert!(played.iter().all(|t| t != "t150"));
    }

    #[test]
    fn a_removed_track_stays_gone_through_shuffle_changes() {
        let mut s = state(120);
        assert!(s.remove_next_track("t3", 0));
        assert!(s.remove_next_track("t110", 0));
        s.fill_up_next_tracks().unwrap();

        s.handle_shuffle(true).unwrap();
        assert!(upcoming(&s).iter().all(|t| t != "t3" && t != "t110"));
        s.handle_shuffle(false).unwrap();
        assert!(upcoming(&s).iter().all(|t| t != "t3" && t != "t110"));

        s.handle_shuffle(true).unwrap();
        let played = play_out(&mut s, 1000);
        assert_eq!(played.len(), 117);
        assert!(played.iter().all(|t| t != "t3" && t != "t110"));
    }

    #[test]
    fn going_back_after_a_reset_skips_removed_tracks() {
        let mut s = state(30);
        assert!(s.remove_next_track("t4", 0));
        s.fill_up_next_tracks().unwrap();
        play_out(&mut s, 7);
        assert_eq!(current(&s), "t8");
        s.reset_playback_to_position(Some(8)).unwrap();
        let back: Vec<_> = s.prev_tracks().iter().map(|t| t.uri.clone()).collect();
        assert_eq!(back, ["t0", "t1", "t2", "t3", "t5", "t6", "t7"]);
    }

    #[test]
    fn starting_over_lands_on_the_first_track_kept() {
        let mut s = state(5);
        s.set_repeat_context(true);
        s.reset_playback_to_position(Some(1)).unwrap();
        assert!(s.remove_next_track("t0", 0), "wrapped into the window");
        s.reset_playback_to_position(None).unwrap();
        assert_eq!(current(&s), "t1");
    }

    #[test]
    fn removing_counts_occurrences_in_play_order() {
        let mut s = state(100);
        queue(&mut s, "t5");
        queue(&mut s, "q");
        // #0 is the queued one, #1 the context's own.
        assert!(s.remove_next_track("t5", 1));
        assert_eq!(upcoming(&s)[..3], ["t5", "q", "t1"]);
        assert!(!upcoming(&s)[2..].contains(&"t5".to_string()));
        assert!(s.remove_next_track("t5", 0));
        assert_eq!(upcoming(&s)[..2], ["q", "t1"]);
        assert!(!s.remove_next_track("t5", 0), "none left");
    }

    #[test]
    fn a_queued_track_waiting_for_room_can_be_removed() {
        let mut s = state(10);
        for n in 0..85 {
            queue(&mut s, format!("q{n}"));
        }
        assert!(s.remove_next_track("q83", 0));
        assert_eq!(s.queue_overflow.len(), 4);
        assert!(play_out(&mut s, 200).iter().all(|t| t != "q83"));
    }

    #[test]
    fn removing_what_isnt_there_says_so() {
        let mut s = state(10);
        play_out(&mut s, 3);
        assert!(!s.remove_next_track("t1", 0), "already played");
        assert!(!s.remove_next_track("nope", 0));
        assert!(!s.remove_next_track("t5", 1), "only one of it");
    }

    #[test]
    fn loading_brings_removed_tracks_back() {
        let mut s = state(10);
        assert!(s.remove_next_track("t4", 0));
        s.forget_removed_tracks();
        s.clear_next_tracks();
        s.reset_playback_to_position(Some(0)).unwrap();
        assert!(upcoming(&s).contains(&"t4".to_string()));
    }

    fn page_uri(i: usize) -> String {
        format!("spotify:track:{i:0>22}")
    }

    /// A list started with one track, under shuffle, growing as a chart does.
    fn growing_chart() -> ConnectState {
        let mut s = state(1);
        s.get_context_mut(ContextType::Default).unwrap().tracks[0].uri = page_uri(0);
        s.player_mut().track.as_mut().unwrap().uri = page_uri(0);
        s.set_shuffle(true);
        s.shuffle_new().unwrap();
        for chunk in [1..11, 11..31, 31..60] {
            let uris: Vec<String> = chunk.map(page_uri).collect();
            s.fill_context_from_page(uris.into()).unwrap();
            s.fill_up_next_tracks().unwrap();
        }
        s
    }

    #[test]
    fn a_chart_started_with_one_track_plays_shuffled() {
        let mut s = growing_chart();
        let played = play_out(&mut s, 100);
        let in_order: Vec<_> = (1..60).map(page_uri).collect();
        assert_eq!(played.len(), 59);
        assert_ne!(played, in_order, "shuffle is on, so not chart order");
        let mut sorted = played.clone();
        sorted.sort();
        assert_eq!(sorted, in_order, "every track, once");
    }

    #[test]
    fn turning_shuffle_off_after_appends_restores_the_chart_order() {
        let mut s = growing_chart();
        s.handle_shuffle(false).unwrap();
        let order: Vec<_> = s.get_context(ContextType::Default).unwrap().tracks.iter().map(|t| t.uri.clone()).collect();
        assert_eq!(order, (0..60).map(page_uri).collect::<Vec<_>>());
        assert_eq!(play_out(&mut s, 100), (1..60).map(page_uri).collect::<Vec<_>>());
    }

    /// Up-next right after each append, the way `handle_append_tracks` does it (BUGS-PLAN
    /// queue audit: the live `--selftest-bp-chart` saw the first appended song twice and
    /// a station's seed song ahead of its station).
    fn one_song_list_then(appends: &[&[usize]], shuffle: bool) -> Vec<Vec<String>> {
        let mut s = state(1);
        s.get_context_mut(ContextType::Default).unwrap().tracks[0].uri = page_uri(0);
        s.player_mut().track.as_mut().unwrap().uri = page_uri(0);
        if shuffle {
            s.set_shuffle(true);
            s.shuffle_new().unwrap();
        }
        let mut seen = Vec::new();
        for chunk in appends {
            let uris: Vec<String> = chunk.iter().copied().map(page_uri).collect();
            s.fill_context_from_page(uris.into()).unwrap();
            s.fill_up_next_tracks().unwrap();
            seen.push(upcoming(&s));
        }
        seen
    }

    #[test]
    fn a_station_appended_to_the_playing_song_follows_it_once() {
        let seen = one_song_list_then(&[&[1, 2, 3]], false);
        assert_eq!(seen[0], [1, 2, 3].map(page_uri));
    }

    #[test]
    fn chart_rows_appended_one_by_one_are_each_up_next_once() {
        let seen = one_song_list_then(&[&[1], &[2], &[3], &[4, 5]], false);
        assert_eq!(seen[0], [1].map(page_uri));
        assert_eq!(seen[1], [1, 2].map(page_uri));
        assert_eq!(seen[3], [1, 2, 3, 4, 5].map(page_uri));
    }

    /// What `Spirc::handle_load_inner` does for a plain track list (`loadtracks`, no
    /// shuffle options): every such list has the same `spotify:web-api` uri.
    fn load_list(s: &mut ConnectState, uris: &[usize]) {
        use crate::protocol::context::Context;
        use crate::state::context::ResetContext;
        s.reset_context(ResetContext::Completely);
        s.reset_options();
        let ctx = Context {
            uri: Some("spotify:web-api".into()),
            url: Some("context://spotify:web-api".into()),
            pages: vec![uris.iter().copied().map(page_uri).collect::<Vec<_>>().into()],
            ..Default::default()
        };
        s.update_context(ctx, ContextType::Default).unwrap();
        s.set_active_context(ContextType::Default);
        s.merge_context(None);
        s.clear_next_tracks();
        s.clear_restrictions();
        s.set_current_track(0).unwrap();
        s.reset_playback_to_position(Some(0)).unwrap();
    }

    fn append(s: &mut ConnectState, uris: &[usize]) -> Vec<String> {
        s.fill_context_from_page(uris.iter().copied().map(page_uri).collect::<Vec<_>>().into()).unwrap();
        s.fill_up_after_append().unwrap();
        upcoming(s)
    }

    #[test]
    fn a_list_loaded_after_another_takes_appends_after_its_song() {
        let mut s = state(3);
        load_list(&mut s, &[0]);
        assert_eq!(append(&mut s, &[1, 2]), [1, 2].map(page_uri));
        // The same song again as a new list (a search song, then its station).
        load_list(&mut s, &[1]);
        assert_eq!(upcoming(&s), Vec::<String>::new());
        assert_eq!(append(&mut s, &[5, 6, 7]), [5, 6, 7].map(page_uri), "the station, not the seed again");
        // A chart: first match, then rows one by one.
        load_list(&mut s, &[10]);
        append(&mut s, &[11]);
        append(&mut s, &[12]);
        assert_eq!(append(&mut s, &[13, 14]), [11, 12, 13, 14].map(page_uri));
    }

    /// Without the delimiters (the Web API and the QUEUE panel don't show them).
    fn songs(v: Vec<String>) -> Vec<String> {
        v.into_iter().filter(|u| u != "spotify:delimiter").collect()
    }

    #[test]
    fn with_repeat_on_appended_songs_come_before_the_list_repeats() {
        // QD1: a chart (or a search song's station) started with repeat-all on.
        let mut s = state(3);
        load_list(&mut s, &[0]);
        s.set_repeat_context(true);
        s.reset_playback_to_position(Some(0)).unwrap();
        let next = songs(append(&mut s, &[1, 2, 3]));
        assert_eq!(next[..3], [1, 2, 3].map(page_uri), "the new songs first: {next:?}");
    }

    #[test]
    fn songs_appended_after_the_list_ended_go_on_from_the_first_new_one() {
        // QD3: the list [0, 1, 2] played out (Spirc's handle_stop), then the chart's next
        // matches arrive; DA11 readies the next song.
        let mut s = state(3);
        load_list(&mut s, &[0, 1, 2]);
        s.next_track().unwrap();
        s.next_track().unwrap();
        assert_eq!(current(&s), page_uri(2));
        s.clear_next_tracks();
        s.reset_playback_to_position(None).unwrap();
        s.fill_context_from_page([3, 4].map(page_uri).to_vec().into()).unwrap();
        assert!(s.go_to_appended(&page_uri(3)).unwrap());
        assert_eq!(upcoming(&s), [4].map(page_uri));
        assert_eq!(current(&s), page_uri(3), "not the list's second song again");
    }

    #[test]
    fn shuffled_appends_never_list_a_song_twice_or_the_playing_one() {
        let seen = one_song_list_then(&[&[1], &[2], &[3], &[4, 5]], true);
        let last = seen.last().unwrap();
        let mut sorted = last.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), last.len(), "no song twice: {last:?}");
        assert!(!last.contains(&page_uri(0)), "not the playing song: {last:?}");
    }
}
