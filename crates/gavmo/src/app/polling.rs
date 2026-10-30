//! The bookkeeping behind [`super::hooks::use_polling`], as plain Rust so it
//! can be unit tested natively. The hook only wires it to Leptos effects,
//! timers and tasks.

/// Proof that a request was started, handed back to [`Poller::finish`] with
/// its reply. Carries the generation it was started in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket(u64);

/// What the hook must do after [`Poller::set_active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Nothing changed.
    None,
    /// Polling started: make this first request now and start the timer.
    Start(Ticket),
    /// Polling stopped: clear the timer, run `on_stop`, and make the final
    /// request if there is one.
    Stop { final_fetch: Option<Ticket> },
}

/// Polling state: whether it is on, whether a request is in flight, and a
/// generation bumped on every start and stop.
///
/// - [`Self::tick`] refuses while a request of the current generation is in
///   flight, so a slow server never gets overlapping requests.
/// - A reply is applied only if its generation is still current, so a reply
///   arriving after a stop (or from before a restart) can't overwrite newer
///   state, and replies can't apply out of order across restarts.
/// - A stale reply doesn't touch the in-flight flag, which belongs to the
///   current generation's request.
#[derive(Debug, Default)]
pub(crate) struct Poller {
    generation: u64,
    in_flight: bool,
    polling: bool,
}

impl Poller {
    /// Follows `active`. Turning on starts a new generation with an
    /// immediate request, not blocked by one still running from before (its
    /// reply will be dropped). Turning off starts a new generation too, with
    /// one final request if `fetch_on_stop`.
    pub(crate) fn set_active(&mut self, active: bool, fetch_on_stop: bool) -> Transition {
        match (active, self.polling) {
            (true, false) => {
                self.polling = true;
                self.new_generation();
                Transition::Start(self.issue())
            }
            (false, true) => {
                self.polling = false;
                self.new_generation();
                Transition::Stop {
                    final_fetch: fetch_on_stop.then(|| self.issue()),
                }
            }
            _ => Transition::None,
        }
    }

    /// A timer tick: a request to make, unless polling is off or one is
    /// already in flight.
    pub(crate) fn tick(&mut self) -> Option<Ticket> {
        (self.polling && !self.in_flight).then(|| self.issue())
    }

    /// Records a finished request; `true` if its reply should be applied.
    pub(crate) fn finish(&mut self, ticket: Ticket) -> bool {
        let current = ticket.0 == self.generation;
        if current {
            self.in_flight = false;
        }
        current
    }

    fn new_generation(&mut self) {
        self.generation += 1;
        self.in_flight = false;
    }

    fn issue(&mut self) -> Ticket {
        self.in_flight = true;
        Ticket(self.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> (Poller, Ticket) {
        let mut poller = Poller::default();
        let Transition::Start(first) = poller.set_active(true, false) else {
            panic!("expected a start");
        };
        (poller, first)
    }

    #[test]
    fn starting_makes_an_immediate_request() {
        let (_, first) = started();
        assert_eq!(first, Ticket(1));
    }

    #[test]
    fn nothing_happens_without_a_change() {
        let mut poller = Poller::default();
        assert_eq!(poller.set_active(false, true), Transition::None);
        assert_eq!(poller.tick(), None);
        assert!(matches!(
            poller.set_active(true, false),
            Transition::Start(_)
        ));
        assert_eq!(poller.set_active(true, false), Transition::None);
    }

    #[test]
    fn ticks_are_skipped_while_a_request_is_in_flight() {
        let (mut poller, first) = started();
        assert_eq!(poller.tick(), None);
        assert!(poller.finish(first));
        let second = poller.tick().expect("free again after the reply");
        assert_eq!(poller.tick(), None);
        assert!(poller.finish(second));
    }

    #[test]
    fn stopping_runs_no_final_request_unless_asked() {
        let (mut poller, _) = started();
        assert_eq!(
            poller.set_active(false, false),
            Transition::Stop { final_fetch: None }
        );
        assert_eq!(poller.tick(), None, "a late timer tick after stop");
    }

    /// The boot console case: the last request in flight while booting ends
    /// is dropped, and only the final fetch is applied.
    #[test]
    fn after_a_stop_only_the_final_request_is_applied() {
        let (mut poller, in_flight) = started();
        let Transition::Stop {
            final_fetch: Some(last),
        } = poller.set_active(false, true)
        else {
            panic!("expected a final fetch");
        };
        assert!(!poller.finish(in_flight), "reply from while running");
        assert!(poller.finish(last));
        assert_eq!(poller.tick(), None);
    }

    /// The stats case: a reply arriving after the VM stopped must not bring
    /// back the stats the stop just cleared.
    #[test]
    fn a_reply_after_a_stop_is_dropped() {
        let (mut poller, in_flight) = started();
        poller.set_active(false, false);
        assert!(!poller.finish(in_flight));
    }

    #[test]
    fn a_restart_is_not_blocked_by_a_request_from_before() {
        let (mut poller, old) = started();
        poller.set_active(false, false);
        let Transition::Start(new) = poller.set_active(true, false) else {
            panic!("expected a start");
        };
        assert_ne!(old, new);
        assert!(!poller.finish(old), "replies can't apply across restarts");
        assert!(poller.finish(new));
    }

    /// A stale reply must not clear the current request's in-flight flag,
    /// or the next tick would overlap it.
    #[test]
    fn a_stale_reply_does_not_unblock_ticks() {
        let (mut poller, old) = started();
        poller.set_active(false, false);
        let Transition::Start(current) = poller.set_active(true, false) else {
            panic!("expected a start");
        };
        assert!(!poller.finish(old));
        assert_eq!(
            poller.tick(),
            None,
            "the current request is still in flight"
        );
        assert!(poller.finish(current));
        assert!(poller.tick().is_some());
    }
}
