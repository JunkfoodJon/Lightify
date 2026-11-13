use crate::{
    core::{Error, Session, error::ErrorKind},
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context,
        transfer_state::TransferState,
    },
    state::{ConnectState, context::ContextType},
};
use std::{
    cmp::PartialEq,
    collections::{HashMap, VecDeque},
    fmt::{Display, Formatter},
    hash::Hash,
    time::Duration,
};
use thiserror::Error as ThisError;
use tokio::time::Instant;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Resolve {
    Uri(String),
    Context(Context),
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) enum ContextAction {
    Append,
    Replace,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct ResolveContext {
    resolve: Resolve,
    fallback: Option<String>,
    update: ContextType,
    action: ContextAction,
}

impl ResolveContext {
    fn append_context(uri: impl Into<String>) -> Self {
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: None,
            update: ContextType::Default,
            action: ContextAction::Append,
        }
    }

    pub fn from_uri(
        uri: impl Into<String>,
        fallback: impl Into<String>,
        update: ContextType,
        action: ContextAction,
    ) -> Self {
        let fallback_uri = fallback.into();
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: (!fallback_uri.is_empty()).then_some(fallback_uri),
            update,
            action,
        }
    }

    pub fn from_context(context: Context, update: ContextType, action: ContextAction) -> Self {
        Self {
            resolve: Resolve::Context(context),
            fallback: None,
            update,
            action,
        }
    }

    /// the uri which should be used to resolve the context, might not be the context uri
    fn resolve_uri(&self) -> Option<&str> {
        // it's important to call this always, or at least for every ResolveContext
        // otherwise we might not even check if we need to fallback and just use the fallback uri
        match self.resolve {
            Resolve::Uri(ref uri) => ConnectState::valid_resolve_uri(uri),
            Resolve::Context(ref ctx) => {
                ConnectState::find_valid_uri(ctx.uri.as_deref(), ctx.pages.first())
            }
        }
        .or(self.fallback.as_deref())
    }

    /// the actual context uri
    fn context_uri(&self) -> &str {
        match self.resolve {
            Resolve::Uri(ref uri) => uri,
            Resolve::Context(ref ctx) => ctx.uri.as_deref().unwrap_or_default(),
        }
    }
}

impl Display for ResolveContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resolve_uri: <{:?}>, context_uri: <{}>, update: <{:?}>",
            self.resolve_uri(),
            self.context_uri(),
            self.update,
        )
    }
}

#[derive(Debug, ThisError)]
enum ContextResolverError {
    #[error("no next context to resolve")]
    NoNext,
    #[error("tried appending context with {0} pages")]
    UnexpectedPagesSize(usize),
    #[error("tried resolving not allowed context: {0:?}")]
    NotAllowedContext(String),
}

impl From<ContextResolverError> for Error {
    fn from(value: ContextResolverError) -> Self {
        Error::failed_precondition(value)
    }
}

pub struct ContextResolver {
    session: Session,
    queue: VecDeque<ResolveContext>,
    unavailable_contexts: HashMap<ResolveContext, Instant>,
}

// time after which an unavailable context is retried
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(3600);

// Lightify patch: short backoffs for re-requesting a context page after a transient
// failure, before it is marked unavailable.
const RESOLVE_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(500), Duration::from_millis(1500)];

// Lightify patch: contexts fetched ahead of a load (`prepare_context`), so a click on a
// list the user is looking at starts its track without first waiting for the list
// (measured at ~0.7 s for a 4,000-track Liked Songs). Kept briefly, so an edit to the
// list made since is picked up again soon; only a few, the lists last opened.
const PREPARED_FOR: Duration = Duration::from_secs(300);
const PREPARED_MAX: usize = 4;

static PREPARED: std::sync::Mutex<Vec<(String, std::time::Instant, Context)>> =
    std::sync::Mutex::new(Vec::new());

/// A context `prepare_context` fetched for `uri` within the last few minutes, taken: it
/// serves the one load it was fetched for, never a later refresh (a playlist edit, a
/// like), which would get the copy from before the change (BUGS-PLAN DA2).
fn take_prepared(uri: &str) -> Option<Context> {
    let mut list = PREPARED.lock().ok()?;
    list.retain(|(_, at, _)| at.elapsed() < PREPARED_FOR);
    let at = list.iter().position(|(u, _, _)| u == uri)?;
    Some(list.remove(at).2)
}

/// A list opened again this soon (a double tap) isn't fetched again.
const PREPARE_AGAIN_AFTER: Duration = Duration::from_secs(15);

fn prepared_within(uri: &str, within: Duration) -> bool {
    PREPARED.lock().is_ok_and(|list| list.iter().any(|(u, at, _)| u == uri && at.elapsed() < within))
}

fn keep_prepared(uri: &str, ctx: Context) {
    if let Ok(mut list) = PREPARED.lock() {
        list.retain(|(u, _, _)| u != uri);
        list.push((uri.to_string(), std::time::Instant::now(), ctx));
        let excess = list.len().saturating_sub(PREPARED_MAX);
        list.drain(..excess);
    }
}

/// Lightify patch: fetch `context_uri` now and keep it, so a load of it in the next few
/// minutes skips the resolve. Costs one context request, no audio key and no Web-API
/// quota; a failure only means the load resolves it itself, as before.
pub async fn prepare_context(session: &Session, context_uri: &str) -> Result<(), Error> {
    // Opened again: fetched again, so the copy has what changed since (a song liked or
    // added); upstream-patch kept the first copy for 5 minutes and played the old list (DA2).
    if prepared_within(context_uri, PREPARE_AGAIN_AFTER) {
        return Ok(());
    }
    let ctx = session.spclient().get_context(context_uri).await?;
    keep_prepared(context_uri, ctx);
    Ok(())
}

impl ContextResolver {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            queue: VecDeque::new(),
            unavailable_contexts: HashMap::new(),
        }
    }

    pub fn add(&mut self, resolve: ResolveContext) {
        let last_try = self
            .unavailable_contexts
            .get(&resolve)
            // Lightify patch: elapsed time since the failed try. Upstream had the operands
            // reversed, which saturates to 0 so RETRY_UNAVAILABLE never fired and a page
            // that failed once was ignored for the session's whole life.
            .map(|i| Instant::now().duration_since(*i));

        let last_try = if matches!(last_try, Some(last_try) if last_try > RETRY_UNAVAILABLE) {
            let _ = self.unavailable_contexts.remove(&resolve);
            debug!(
                "context was requested {}s ago, trying again to resolve the requested context",
                last_try.expect("checked by condition").as_secs()
            );
            None
        } else {
            last_try
        };

        if last_try.is_some() {
            debug!("tried loading unavailable context: {resolve}");
            return;
        } else if self.queue.contains(&resolve) {
            debug!("update for {resolve} is already added");
            return;
        } else {
            trace!(
                "added {} to resolver queue",
                resolve.resolve_uri().unwrap_or(resolve.context_uri())
            )
        }

        self.queue.push_back(resolve)
    }

    pub fn add_list(&mut self, resolve: Vec<ResolveContext>) {
        for resolve in resolve {
            self.add(resolve)
        }
    }

    pub fn remove_used_and_invalid(&mut self) {
        if let Some((_, _, remove)) = self.find_next() {
            let _ = self.queue.drain(0..remove); // remove invalid
        }
        self.queue.pop_front(); // remove used
    }

    pub fn clear(&mut self) {
        self.queue = VecDeque::new()
    }

    fn find_next(&self) -> Option<(&ResolveContext, &str, usize)> {
        for idx in 0..self.queue.len() {
            let next = self.queue.get(idx)?;
            match next.resolve_uri() {
                None => {
                    warn!("skipped {idx} because of invalid resolve_uri: {next}");
                    continue;
                }
                Some(uri) => return Some((next, uri, idx)),
            }
        }
        None
    }

    pub fn has_next(&self) -> bool {
        self.find_next().is_some()
    }

    pub async fn get_next_context(
        &self,
        recent_track_uri: impl Fn() -> Vec<String>,
    ) -> Result<Context, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;
        let recent = Self::recent_for(next, recent_track_uri);
        Self::resolve(self.session.clone(), next.clone(), recent).await
    }

    /// Lightify patch (BUGS-PLAN DA3): the next resolve as a future that owns everything it
    /// needs, so Spirc runs it as its own task. Built inside its `select!` loop, it was
    /// dropped whenever another branch fired first (a player event every 500 ms on the
    /// desktop), so a page or autoplay request slower than that never finished, and the
    /// retry backoff never elapsed.
    pub fn next_request(
        &self,
        recent_track_uri: impl Fn() -> Vec<String>,
    ) -> Option<(ResolveContext, impl std::future::Future<Output = Result<Context, Error>> + Send + 'static)> {
        let (next, _, _) = self.find_next()?;
        let recent = Self::recent_for(next, recent_track_uri);
        Some((next.clone(), Self::resolve(self.session.clone(), next.clone(), recent)))
    }

    /// Whether `resolve` is still the next one queued: a result for one that a new load
    /// replaced meanwhile is dropped (Lightify patch, DA3).
    pub fn next_is(&self, resolve: &ResolveContext) -> bool {
        self.find_next().is_some_and(|(next, _, _)| next == resolve)
    }

    fn recent_for(next: &ResolveContext, recent_track_uri: impl Fn() -> Vec<String>) -> Vec<String> {
        match next.update {
            ContextType::Autoplay => recent_track_uri(),
            ContextType::Default => Vec::new(),
        }
    }

    async fn resolve(session: Session, next: ResolveContext, recent: Vec<String>) -> Result<Context, Error> {
        let resolve_uri = next.resolve_uri().ok_or(ContextResolverError::NoNext)?.to_string();
        let resolve_uri = resolve_uri.as_str();
        match next.update {
            ContextType::Default => {
                // Lightify patch: a context fetched ahead of this load needs no request.
                let mut ctx = match take_prepared(resolve_uri) {
                    Some(ctx) => {
                        debug!("{resolve_uri} was prepared ahead, no resolve needed");
                        Ok(ctx)
                    }
                    None => session.spclient().get_context(resolve_uri).await,
                };
                for backoff in RESOLVE_RETRY_BACKOFF {
                    match &ctx {
                        Err(why) if Self::is_transient(why) => {
                            debug!("resolving {resolve_uri} failed ({why}), retrying in {backoff:?}");
                            tokio::time::sleep(backoff).await;
                            ctx = session.spclient().get_context(resolve_uri).await;
                        }
                        _ => break,
                    }
                }
                if let Ok(ctx) = ctx.as_mut() {
                    ctx.uri = Some(next.context_uri().to_string());
                    ctx.url = ctx.uri.as_ref().map(|s| format!("context://{s}"));
                }

                ctx
            }
            ContextType::Autoplay => {
                if resolve_uri.contains("spotify:show:") || resolve_uri.contains("spotify:episode:")
                {
                    // autoplay is not supported for podcasts
                    Err(ContextResolverError::NotAllowedContext(
                        resolve_uri.to_string(),
                    ))?
                }

                let request = AutoplayContextRequest {
                    context_uri: Some(resolve_uri.to_string()),
                    recent_track_uri: recent,
                    ..Default::default()
                };
                session.spclient().get_autoplay_context(&request).await
            }
        }
    }

    // Lightify patch: errors that a retry can't fix (missing, forbidden, malformed).
    fn is_transient(why: &Error) -> bool {
        !matches!(
            why.kind,
            ErrorKind::NotFound
                | ErrorKind::PermissionDenied
                | ErrorKind::InvalidArgument
                | ErrorKind::FailedPrecondition
        )
    }

    pub fn mark_next_unavailable(&mut self) {
        if let Some((next, _, _)) = self.find_next() {
            self.unavailable_contexts
                .insert(next.clone(), Instant::now());
        }
    }

    pub fn apply_next_context(
        &self,
        state: &mut ConnectState,
        mut context: Context,
    ) -> Result<Option<Vec<ResolveContext>>, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        let remaining = match next.action {
            ContextAction::Append if context.pages.len() == 1 => state
                .fill_context_from_page(context.pages.remove(0))
                .map(|_| None),
            ContextAction::Replace => {
                let remaining = state.update_context(context, next.update);
                if let Resolve::Context(ref ctx) = next.resolve {
                    state.merge_context(ctx.pages.clone().pop());
                }

                remaining
            }
            ContextAction::Append => {
                warn!("unexpected page size: {context:#?}");
                Err(ContextResolverError::UnexpectedPagesSize(context.pages.len()).into())
            }
        }?;

        Ok(remaining.map(|remaining| {
            remaining
                .into_iter()
                .map(ResolveContext::append_context)
                .collect::<Vec<_>>()
        }))
    }

    pub fn try_finish(
        &self,
        state: &mut ConnectState,
        transfer_state: &mut Option<TransferState>,
    ) -> bool {
        let (next, _, _) = match self.find_next() {
            None => return false,
            Some(next) => next,
        };

        // when there is only one update type, we are the last of our kind, so we should update the state
        if self
            .queue
            .iter()
            .filter(|resolve| resolve.update == next.update)
            .count()
            != 1
        {
            return false;
        }

        match (next.update, state.active_context) {
            (ContextType::Default, ContextType::Default) | (ContextType::Autoplay, _) => {
                debug!(
                    "last item of type <{:?}>, finishing state setup",
                    next.update
                );
            }
            (ContextType::Default, _) => {
                debug!("skipped finishing default, because it isn't the active context");
                return false;
            }
        }

        let active_ctx = state.get_context(state.active_context);
        let res = if let Some(transfer_state) = transfer_state.take() {
            state.finish_transfer(transfer_state)
        } else if state.shuffling_context() && next.update == ContextType::Default {
            state.shuffle_new()
        } else if matches!(active_ctx, Ok(ctx) if ctx.index.track == 0) {
            // has context, and context is not touched
            // when the index is not zero, the next index was already evaluated elsewhere
            let ctx = active_ctx.expect("checked by precondition");
            let idx = ConnectState::find_index_in_context(ctx, |t| {
                state.current_track(|c| t.uri == c.uri)
            })
            .ok();

            state.reset_playback_to_position(idx)
        } else {
            state.fill_up_next_tracks()
        };

        if let Err(why) = res {
            error!("setup of state failed: {why}, last used resolve {next:#?}")
        }

        state.update_restrictions();
        state.update_queue_revision();

        true
    }
}


// Lightify patch: the contexts kept by `prepare_context`.
#[cfg(test)]
mod prepared_tests {
    use super::*;

    fn ctx(uri: &str) -> Context {
        Context { uri: Some(uri.to_string()), ..Default::default() }
    }

    #[test]
    fn a_prepared_context_is_found_and_only_the_last_few_are_kept() {
        for n in 0..PREPARED_MAX + 2 {
            keep_prepared(&format!("spotify:playlist:p{n}"), ctx(&format!("spotify:playlist:p{n}")));
        }
        // The oldest two made room for the newest.
        assert!(take_prepared("spotify:playlist:p0").is_none());
        assert!(take_prepared("spotify:playlist:p1").is_none());
        let last = format!("spotify:playlist:p{}", PREPARED_MAX + 1);
        // Offering the same list again replaces it rather than keeping two.
        keep_prepared(&last, ctx("spotify:playlist:fresh"));
        assert!(prepared_within(&last, PREPARE_AGAIN_AFTER), "just prepared: not fetched again on a double open");
        assert_eq!(take_prepared(&last).and_then(|c| c.uri).as_deref(), Some("spotify:playlist:fresh"));
        assert!(take_prepared("spotify:playlist:never").is_none());
    }

    /// DA2: a prepared copy serves the one load it was made for, never a later refresh
    /// (a like or an edit afterwards would be answered with the copy from before).
    #[test]
    fn a_prepared_context_serves_one_load_only() {
        keep_prepared("spotify:playlist:once", ctx("spotify:playlist:once"));
        assert!(take_prepared("spotify:playlist:once").is_some(), "the load gets it");
        assert!(take_prepared("spotify:playlist:once").is_none(), "a refresh after it resolves again");
        assert!(!prepared_within("spotify:playlist:once", PREPARE_AGAIN_AFTER), "opened again: fetched again");
    }
}
