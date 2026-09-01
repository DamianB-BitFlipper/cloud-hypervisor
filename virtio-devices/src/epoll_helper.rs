// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2020 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use log::{error, info};
use thiserror::Error;
use vmm_sys_util::eventfd::EventFd;

pub struct EpollHelper {
    pause_evt: EventFd,
    epoll_file: File,
}

#[derive(Error, Debug)]
pub enum EpollHelperError {
    #[error("Failed to create Fd")]
    CreateFd(#[source] std::io::Error),
    #[error("Failed to epoll_ctl")]
    Ctl(#[source] std::io::Error),
    #[error("IO error")]
    IoError(#[source] std::io::Error),
    #[error("Failed to epoll_wait")]
    Wait(#[source] std::io::Error),
    #[error("Failed to get virtio-queue index")]
    QueueRingIndex(#[source] virtio_queue::Error),
    #[error("Failed to handle virtio device events")]
    HandleEvent(#[source] anyhow::Error),
    #[error("Failed to handle timeout")]
    HandleTimeout(#[source] anyhow::Error),
}

pub const EPOLL_HELPER_EVENT_PAUSE: u16 = 0;
pub const EPOLL_HELPER_EVENT_KILL: u16 = 1;
pub const EPOLL_HELPER_EVENT_LAST: u16 = 15;

pub trait EpollHelperHandler {
    // Handle one event at a time. The EpollHelper iterates over a list of
    // events that have been returned by epoll_wait(). For each event, the
    // current method is invoked to let the implementation decide how to process
    // the incoming event.
    fn handle_event(
        &mut self,
        helper: &mut EpollHelper,
        event: &epoll::Event,
    ) -> Result<(), EpollHelperError>;

    // This method is only invoked if the EpollHelper was configured to call
    // epoll_wait() with a valid timeout (different from -1), meaning the call
    // won't block forever. When the timeout is reached, and if no even has been
    // triggered, this function will be called to let the implementation decide
    // how to interpret such situation. By default, it provides a no-op
    // implementation.
    fn handle_timeout(&mut self, _helper: &mut EpollHelper) -> Result<(), EpollHelperError> {
        Ok(())
    }

    // In some situations, it might be useful to know the full list of events
    // triggered while waiting on epoll_wait(). And having this list provided
    // prior to the iterations over each event might help make some informed
    // decisions. This function should not replace handle_event(), otherwise it
    // would completely defeat the purpose of having the loop being factorized
    // through the EpollHelper structure.
    fn event_list(
        &mut self,
        _helper: &mut EpollHelper,
        _events: &[epoll::Event],
    ) -> Result<(), EpollHelperError> {
        Ok(())
    }

    // Invoked when a PAUSE event is received, before the pause is
    // acknowledged through the paused_sync barrier. This gives the
    // implementation a chance to quiesce in-flight work — e.g. drain
    // outstanding asynchronous I/O — so the device reaches a self-
    // consistent state before the VMM's pause() returns. Devices are
    // snapshotted while paused: anything still in flight after the barrier
    // releases (kernel DMA into guest memory, deferred used-ring updates)
    // is torn across the serialized device state and the memory image, and
    // restored clones see a corrupt virtqueue. By default there is nothing
    // to quiesce.
    fn quiesce(&mut self, _helper: &mut EpollHelper) -> Result<(), EpollHelperError> {
        Ok(())
    }

    // Return true when quiesce() waits for events from the helper's epoll set.
    // The pause event is temporarily removed for such handlers so its shared,
    // level-triggered eventfd cannot starve the events quiesce is waiting for.
    fn quiesce_waits_for_epoll_events(&self) -> bool {
        false
    }
}

impl EpollHelper {
    pub fn new(
        kill_evt: &EventFd,
        pause_evt: &EventFd,
    ) -> std::result::Result<Self, EpollHelperError> {
        // Create the epoll file descriptor
        let epoll_fd = epoll::create(true).map_err(EpollHelperError::CreateFd)?;
        // Use 'File' to enforce closing on 'epoll_fd'
        // SAFETY: epoll_fd is a valid fd
        let epoll_file = unsafe { File::from_raw_fd(epoll_fd) };

        let mut helper = Self {
            pause_evt: pause_evt.try_clone().unwrap(),
            epoll_file,
        };

        helper.add_event(kill_evt.as_raw_fd(), EPOLL_HELPER_EVENT_KILL)?;
        helper.add_event(helper.pause_evt.as_raw_fd(), EPOLL_HELPER_EVENT_PAUSE)?;
        Ok(helper)
    }

    pub fn add_event(&mut self, fd: RawFd, id: u16) -> std::result::Result<(), EpollHelperError> {
        self.add_event_custom(fd, id, epoll::Events::EPOLLIN)
    }

    pub fn add_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        epoll::ctl(
            self.epoll_file.as_raw_fd(),
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd,
            epoll::Event::new(evts, id.into()),
        )
        .map_err(EpollHelperError::Ctl)
    }

    pub fn mod_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        epoll::ctl(
            self.epoll_file.as_raw_fd(),
            epoll::ControlOptions::EPOLL_CTL_MOD,
            fd,
            epoll::Event::new(evts, id.into()),
        )
        .map_err(EpollHelperError::Ctl)
    }

    pub fn del_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        epoll::ctl(
            self.epoll_file.as_raw_fd(),
            epoll::ControlOptions::EPOLL_CTL_DEL,
            fd,
            epoll::Event::new(evts, id.into()),
        )
        .map_err(EpollHelperError::Ctl)
    }

    pub fn wait_for_events(
        &self,
        timeout: i32,
        events: &mut [epoll::Event],
    ) -> std::result::Result<usize, EpollHelperError> {
        loop {
            match epoll::wait(self.epoll_file.as_raw_fd(), timeout, events) {
                Ok(num_events) => return Ok(num_events),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(EpollHelperError::Wait(e)),
            }
        }
    }

    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
    ) -> std::result::Result<(), EpollHelperError> {
        self.run_with_timeout(paused, paused_sync, handler, -1, false)
    }

    #[cfg(not(fuzzing))]
    pub fn run_with_timeout(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
        timeout: i32,
        enable_event_list: bool,
    ) -> std::result::Result<(), EpollHelperError> {
        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        // Before jumping into the epoll loop, check if the device is expected
        // to be in a paused state. This is helpful for the restore code path
        // as the device thread should not start processing anything before the
        // device has been resumed.
        while paused.load(Ordering::SeqCst) {
            thread::park();
        }

        loop {
            let num_events =
                match epoll::wait(self.epoll_file.as_raw_fd(), timeout, &mut events[..]) {
                    Ok(res) => res,
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            // It's well defined from the epoll_wait() syscall
                            // documentation that the epoll loop can be interrupted
                            // before any of the requested events occurred or the
                            // timeout expired. In both those cases, epoll_wait()
                            // returns an error of type EINTR, but this should not
                            // be considered as a regular error. Instead it is more
                            // appropriate to retry, by calling into epoll_wait().
                            continue;
                        }
                        return Err(EpollHelperError::Wait(e));
                    }
                };

            if num_events == 0 {
                // This case happens when the timeout is reached before any of
                // the registered events is triggered.
                handler.handle_timeout(self)?;
                continue;
            }

            if enable_event_list {
                handler.event_list(self, &events[..num_events])?;
            }

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    EPOLL_HELPER_EVENT_KILL => {
                        info!("KILL_EVENT received, stopping epoll loop");
                        return Ok(());
                    }
                    EPOLL_HELPER_EVENT_PAUSE => {
                        info!("PAUSE_EVENT received, pausing epoll loop");

                        // The shared pause eventfd stays readable until every
                        // device thread has acknowledged the pause. Remove it
                        // from this thread's epoll set while quiescing so the
                        // handler can wait on its ordinary completion events
                        // without busy-spinning on the pause notification.
                        let quiesce_waits_for_epoll_events =
                            handler.quiesce_waits_for_epoll_events();
                        let pause_event_removed = if quiesce_waits_for_epoll_events {
                            match self.del_event_custom(
                                self.pause_evt.as_raw_fd(),
                                EPOLL_HELPER_EVENT_PAUSE,
                                epoll::Events::EPOLLIN,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!(
                                        "Failed to suspend pause event while quiescing: {e:?}"
                                    );
                                    false
                                }
                            }
                        } else {
                            false
                        };

                        // Quiesce in-flight work before acknowledging the
                        // pause. A quiesce failure is logged rather than
                        // propagated: the barrier below must always be
                        // reached or the VMM's pause() call deadlocks, and
                        // parking with residual in-flight work matches the
                        // previous behavior.
                        let can_quiesce = !quiesce_waits_for_epoll_events || pause_event_removed;
                        if can_quiesce
                            && let Err(e) = handler.quiesce(self)
                        {
                            error!("Failed to quiesce handler before pause: {e:?}");
                        }

                        // Acknowledge the pause is effective by using the
                        // paused_sync barrier.
                        paused_sync.wait();

                        // We loop here to handle spurious park() returns.
                        // Until we have not resumed, the paused boolean will
                        // be true.
                        while paused.load(Ordering::SeqCst) {
                            thread::park();
                        }

                        // Drain pause event after the device has been resumed.
                        // This ensures the pause event has been seen by each
                        // thread related to this virtio device.
                        let _ = self.pause_evt.read();
                        if pause_event_removed {
                            self.add_event(self.pause_evt.as_raw_fd(), EPOLL_HELPER_EVENT_PAUSE)?;

                            // Discard the rest of this epoll batch. Events
                            // returned alongside PAUSE may have been consumed
                            // by quiesce and must be observed again from current
                            // fd readiness.
                            break;
                        }
                    }
                    _ => {
                        handler.handle_event(self, event)?;
                    }
                }
            }
        }
    }

    #[cfg(fuzzing)]
    // Require to have a 'queue_evt' being kicked before calling
    // and return when no epoll events are active
    pub fn run_with_timeout(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
        _timeout: i32,
        _enable_event_list: bool,
    ) -> std::result::Result<(), EpollHelperError> {
        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        loop {
            let num_events = match epoll::wait(self.epoll_file.as_raw_fd(), 0, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(EpollHelperError::Wait(e));
                }
            };

            // Return when no epoll events are active
            if num_events == 0 {
                return Ok(());
            }

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    EPOLL_HELPER_EVENT_KILL => {
                        info!("KILL_EVENT received, stopping epoll loop");
                        return Ok(());
                    }
                    EPOLL_HELPER_EVENT_PAUSE => {
                        info!("PAUSE_EVENT received, pausing epoll loop");

                        let quiesce_waits_for_epoll_events =
                            handler.quiesce_waits_for_epoll_events();
                        let pause_event_removed = if quiesce_waits_for_epoll_events {
                            match self.del_event_custom(
                                self.pause_evt.as_raw_fd(),
                                EPOLL_HELPER_EVENT_PAUSE,
                                epoll::Events::EPOLLIN,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!(
                                        "Failed to suspend pause event while quiescing: {e:?}"
                                    );
                                    false
                                }
                            }
                        } else {
                            false
                        };

                        // Quiesce in-flight work before acknowledging the
                        // pause. A quiesce failure is logged rather than
                        // propagated: the barrier below must always be
                        // reached or the VMM's pause() call deadlocks, and
                        // parking with residual in-flight work matches the
                        // previous behavior.
                        let can_quiesce = !quiesce_waits_for_epoll_events || pause_event_removed;
                        if can_quiesce
                            && let Err(e) = handler.quiesce(self)
                        {
                            error!("Failed to quiesce handler before pause: {e:?}");
                        }

                        // Acknowledge the pause is effective by using the
                        // paused_sync barrier.
                        paused_sync.wait();

                        // We loop here to handle spurious park() returns.
                        // Until we have not resumed, the paused boolean will
                        // be true.
                        while paused.load(Ordering::SeqCst) {
                            thread::park();
                        }

                        // Drain pause event after the device has been resumed.
                        // This ensures the pause event has been seen by each
                        // thread related to this virtio device.
                        let _ = self.pause_evt.read();
                        if pause_event_removed {
                            self.add_event(self.pause_evt.as_raw_fd(), EPOLL_HELPER_EVENT_PAUSE)?;
                            break;
                        }
                    }
                    _ => {
                        handler.handle_event(self, event)?;
                    }
                }
            }
        }
    }
}

impl AsRawFd for EpollHelper {
    fn as_raw_fd(&self) -> RawFd {
        self.epoll_file.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use super::*;

    const TEST_COMPLETION_EVENT: u16 = EPOLL_HELPER_EVENT_LAST + 1;

    struct QuiescingHandler {
        completion_evt: EventFd,
        quiesce_count: Arc<AtomicUsize>,
        quiesce_started_tx: Sender<()>,
        handled_tx: Sender<()>,
    }

    impl EpollHelperHandler for QuiescingHandler {
        fn handle_event(
            &mut self,
            _helper: &mut EpollHelper,
            event: &epoll::Event,
        ) -> Result<(), EpollHelperError> {
            if event.data as u16 != TEST_COMPLETION_EVENT {
                return Err(EpollHelperError::IoError(std::io::Error::other(
                    "unexpected test event",
                )));
            }
            self.completion_evt
                .read()
                .map_err(EpollHelperError::IoError)?;
            self.handled_tx.send(()).unwrap();
            Ok(())
        }

        fn quiesce(&mut self, helper: &mut EpollHelper) -> Result<(), EpollHelperError> {
            self.quiesce_started_tx.send(()).unwrap();
            let mut events = [epoll::Event::new(epoll::Events::empty(), 0); 1];
            let num_events = helper.wait_for_events(2_000, &mut events)?;
            if num_events != 1 || events[0].data as u16 != TEST_COMPLETION_EVENT {
                return Err(EpollHelperError::IoError(std::io::Error::other(
                    "completion event did not wake quiesce",
                )));
            }
            self.completion_evt
                .read()
                .map_err(EpollHelperError::IoError)?;
            self.quiesce_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn quiesce_waits_for_epoll_events(&self) -> bool {
            true
        }
    }

    #[test]
    fn test_pause_quiesce_uses_epoll_and_rearms_pause_event() {
        let kill_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let pause_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let completion_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let mut helper = EpollHelper::new(&kill_evt, &pause_evt).unwrap();
        helper
            .add_event(completion_evt.as_raw_fd(), TEST_COMPLETION_EVENT)
            .unwrap();

        let paused = Arc::new(AtomicBool::new(false));
        let paused_sync = Arc::new(Barrier::new(2));
        let quiesce_count = Arc::new(AtomicUsize::new(0));
        let (quiesce_started_tx, quiesce_started_rx) = mpsc::channel();
        let (handled_tx, handled_rx) = mpsc::channel();
        let mut handler = QuiescingHandler {
            completion_evt: completion_evt.try_clone().unwrap(),
            quiesce_count: Arc::clone(&quiesce_count),
            quiesce_started_tx,
            handled_tx,
        };
        let thread_paused = Arc::clone(&paused);
        let thread_paused_sync = Arc::clone(&paused_sync);
        let worker =
            thread::spawn(move || helper.run(&thread_paused, &thread_paused_sync, &mut handler));

        // Prove the worker entered its epoll loop before initiating pause;
        // workers created for an already-paused restore intentionally park
        // before observing events.
        completion_evt.write(1).unwrap();
        handled_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        for expected_count in 1usize..=2 {
            paused.store(true, Ordering::SeqCst);
            pause_evt.write(1).unwrap();
            quiesce_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            completion_evt.write(1).unwrap();
            paused_sync.wait();
            assert_eq!(quiesce_count.load(Ordering::SeqCst), expected_count);

            paused.store(false, Ordering::SeqCst);
            worker.thread().unpark();

            // A normal event proves the worker resumed and re-armed its epoll
            // set before the next pause cycle.
            completion_evt.write(1).unwrap();
            handled_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }

        kill_evt.write(1).unwrap();
        worker.join().unwrap().unwrap();
    }
}
