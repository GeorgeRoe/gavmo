//! Small reactive helpers shared by the components.

use std::future::Future;
use std::time::Duration;

use leptos::prelude::*;

use super::transitions::go_to_login;
use crate::types::AppError;

/// Polls while `active()` is true: one request immediately, then one every
/// `period`.
///
/// - A tick is skipped while the previous request is still in flight, so a
///   slow server never gets a pile-up of overlapping requests.
/// - Every start/stop bumps a generation; a response from an older
///   generation is dropped. So a reply arriving after polling stopped (e.g.
///   stats for a VM that was just stopped) can't overwrite the cleared
///   state, and replies can't apply out of order across restarts.
/// - On stop, `on_stop` runs; with `fetch_on_stop`, one final request is
///   made and applied (e.g. the last lines of the boot console).
/// - Everything stops when the owning component unmounts.
///
/// `fetch` returning `None` means "nothing to apply this time".
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
    let generation = StoredValue::new(0u64);
    let in_flight = StoredValue::new(false);
    let interval = StoredValue::new(None::<IntervalHandle>);

    let tick = move || {
        if in_flight.get_value() {
            return;
        }
        in_flight.set_value(true);
        let started_in = generation.get_value();
        leptos::task::spawn_local(async move {
            let value = fetch().await;
            in_flight.set_value(false);
            if generation.get_value() == started_in
                && let Some(value) = value
            {
                apply(value);
            }
        });
    };

    Effect::new(move |_| {
        let want = active();
        let polling = interval.get_value().is_some();
        if want && !polling {
            generation.update_value(|g| *g += 1);
            // A request from the previous generation may still be running;
            // let it finish (it will be dropped) instead of skipping ticks.
            in_flight.set_value(false);
            tick();
            interval.set_value(set_interval_with_handle(tick, period).ok());
        } else if !want && polling {
            if let Some(handle) = interval.get_value() {
                handle.clear();
            }
            interval.set_value(None);
            generation.update_value(|g| *g += 1);
            in_flight.set_value(false);
            on_stop();
            if fetch_on_stop {
                tick();
            }
        }
    });
    on_cleanup(move || {
        if let Some(handle) = interval.get_value() {
            handle.clear();
        }
    });
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
