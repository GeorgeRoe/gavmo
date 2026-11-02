//! Small reactive helpers shared by the components.

use std::future::Future;
use std::time::Duration;

use leptos::prelude::*;

use super::polling::{Poller, Ticket, Transition};
use super::transitions::go_to_login;
use crate::types::AppError;

/// Polls while `active()` is true: one request immediately, then one every
/// `period`.
///
/// - A tick is skipped while the previous request is still in flight, so a
///   slow server never gets a pile-up of overlapping requests.
/// - A reply from before the latest start/stop is dropped, so one arriving
///   after polling stopped (e.g. stats for a VM that was just stopped) can't
///   overwrite the cleared state.
/// - On stop, `on_stop` runs; with `fetch_on_stop`, one final request is
///   made and applied (e.g. the last lines of the boot console).
/// - Everything stops when the owning component unmounts, and replies
///   arriving after that are dropped.
///
/// The bookkeeping is [`Poller`]; this only wires it to Leptos. `fetch`
/// returning `None` means "nothing to apply this time".
pub(crate) fn use_polling<T, Fut>(
    active: impl Fn() -> bool + 'static,
    period: Duration,
    fetch: impl Fn() -> Fut + Copy + 'static,
    apply: impl Fn(T) + Copy + 'static,
    on_stop: impl Fn() + 'static,
    fetch_on_stop: bool,
) where
    T: 'static,
    Fut: Future<Output = Option<T>> + 'static,
{
    let poller = StoredValue::new(Poller::default());
    let interval = StoredValue::new(None::<IntervalHandle>);

    let run = move |ticket: Ticket| {
        leptos::task::spawn_local(async move {
            let value = fetch().await;
            // `None` once the component is gone.
            let current = poller.try_update_value(|p| p.finish(ticket)) == Some(true);
            if current && let Some(value) = value {
                apply(value);
            }
        });
    };
    let tick = move || {
        if let Some(ticket) = poller.try_update_value(Poller::tick).flatten() {
            run(ticket);
        }
    };
    let clear_interval = move || {
        if let Some(handle) = interval.try_update_value(Option::take).flatten() {
            handle.clear();
        }
    };

    Effect::new(move |_| {
        let want = active();
        match poller
            .try_update_value(|p| p.set_active(want, fetch_on_stop))
            .unwrap_or(Transition::None)
        {
            Transition::None => {}
            Transition::Start(first) => {
                run(first);
                interval.set_value(set_interval_with_handle(tick, period).ok());
            }
            Transition::Stop { final_fetch } => {
                clear_interval();
                on_stop();
                if let Some(last) = final_fetch {
                    run(last);
                }
            }
        }
    });
    on_cleanup(clear_interval);
}

/// Runs a user-triggered server call: sets `busy` for its duration, sends
/// the browser to log in if the session expired, then hands the outcome to
/// `done`.
pub(crate) fn run_action<T: 'static>(
    busy: RwSignal<bool>,
    call: impl Future<Output = Result<T, AppError>> + 'static,
    done: impl FnOnce(Result<T, AppError>) + 'static,
) {
    busy.set(true);
    leptos::task::spawn_local(async move {
        let outcome = call.await;
        busy.set(false);
        if matches!(outcome, Err(AppError::Unauthorized)) {
            go_to_login();
        }
        done(outcome);
    });
}

#[cfg(test)]
#[cfg(feature = "ssr")]
mod tests {
    use super::*;

    /// `busy` covers exactly the call, and `done` gets its outcome; an
    /// expired session also sends the browser to log in (a no-op here).
    #[tokio::test]
    async fn run_action_marks_busy_until_the_call_finishes() {
        _ = any_spawner::Executor::init_tokio();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let owner = Owner::new();
                owner.set();
                for outcome in [Ok(7), Err(AppError::Unauthorized)] {
                    let busy = RwSignal::new(false);
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let expected = outcome.clone();
                    run_action(busy, async move { outcome }, move |got| {
                        tx.send(got).unwrap();
                    });
                    assert!(busy.get_untracked());
                    assert_eq!(rx.await.unwrap(), expected);
                    assert!(!busy.get_untracked());
                }
            })
            .await;
    }
}
