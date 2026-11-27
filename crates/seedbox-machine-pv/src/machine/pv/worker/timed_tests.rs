//! Backend-neutral checks of [`Machine::run_timed`], which each scripted
//! backend's tests run with their own guest scripts.
use super::{Backend, Machine, Stop};
use std::fmt::Debug;

/// Run one machine from `make` through `run_timed(0, upto)` with `frames`,
/// and another through the runs a client would make with one run per input:
/// to each frame's time, delivering it in a run from there, and from a block
/// on at its wake or the next frame, whichever comes first. Both must stop
/// the same way, deliver the same frames and leave the same machine, as
/// `look` sees it. Returns the stop and how many frames were delivered.
pub(super) fn a_timed_run_matches_one_run_per_input<B: Backend, T: PartialEq + Debug>(
    make: impl Fn() -> Machine<B>,
    frames: &[(u64, Vec<u8>)],
    upto: u64,
    look: impl Fn(&Machine<B>) -> T,
) -> (Stop, usize) {
    let mut split = make();
    let mut pending = frames.iter().cloned().peekable();
    let mut due = Vec::new();
    let mut now = 0;
    let mut delivered = 0;
    let stop = loop {
        let next = pending.peek().map(|&(at, _)| at);
        delivered += due.len();
        let stop = split
            .run(
                now,
                next.unwrap_or(upto),
                1000,
                false,
                &[],
                std::mem::take(&mut due),
                &[],
            )
            .unwrap();
        let resume = match stop {
            Stop::Horizon => next.filter(|&at| at == split.state.now),
            Stop::Blocked(wake) => {
                let wake = wake.filter(|&wake| wake > split.state.now && wake < upto);
                match (wake, next) {
                    (Some(wake), Some(at)) => Some(wake.min(at)),
                    (wake, at) => wake.or(at),
                }
            }
            _ => None,
        };
        let Some(at) = resume else { break stop };
        while let Some((_, bytes)) = pending.next_if(|&(due, _)| due == at) {
            due.push(bytes);
        }
        now = at;
    };
    let mut timed = make();
    let result = timed
        .run_timed(0, upto, 1000, false, &[], Vec::new(), frames.to_vec(), &[])
        .unwrap();
    assert_eq!(result, (stop, delivered));
    assert_eq!(look(&timed), look(&split));
    result
}

/// A timed run refuses frames that are not inside it, in order, before it
/// runs the guest.
pub(super) fn a_timed_run_refuses_frames_outside_it<B: Backend>(make: impl Fn() -> Machine<B>) {
    let frame = vec![0xff; 60];
    for frames in [
        vec![(0, frame.clone())],
        vec![(100, frame.clone())],
        vec![(20, frame.clone()), (10, frame.clone())],
    ] {
        let mut vm = make();
        let error = vm
            .run_timed(0, 100, 1000, false, &[], Vec::new(), frames, &[])
            .unwrap_err();
        assert!(error.contains("outside their run"), "{error}");
    }
}
