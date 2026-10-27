/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Adapted from swarm-scheduler-exporter/src/test_support/fake_docker.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! A [`DockerApi`] answering from fields a test sets: the port of the Go daemon tests'
//! `fakeDocker`, which records every call, can fail or hang the list, hang an `events` call, and
//! hands the first stream the events a test sends while every later stream stays open and silent
//! until it is dropped.

use std::collections::{BTreeSet, HashMap};
use std::future::{Future, pending};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use tokio::sync::mpsc;

use crate::clock::Clock;
use crate::docker::context::{self, ContextError};
use crate::docker::types::{ContainerInspect, ContainerSummary, Event};
use crate::docker::{DockerApi, Error, EventStream, Filters, StreamError, StreamErrorKind};

/// What one fake event stream yields: an event, or the error that ends the stream.
pub(crate) type FakeEvent = Result<Event, StreamError>;

/// One call the fake answered (or is answering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Ping,
    List,
    Inspect(String),
    /// An `events` call, with its `since`.
    Events(Timestamp),
}

/// The fake's answers, set before it is shared; what it records is behind locks and atomics.
#[derive(Default)]
pub(crate) struct FakeDocker {
    pub(crate) containers: Vec<ContainerSummary>,
    pub(crate) inspects: HashMap<String, ContainerInspect>,
    /// How many lists fail (`list failed`) before one answers.
    pub(crate) list_failures: AtomicUsize,
    /// `container_list` never answers, as a wedged daemon: only the caller's deadline ends it.
    pub(crate) list_blocks: bool,
    /// `container_inspect` never answers; how its context ended is recorded.
    pub(crate) inspect_blocks: bool,
    pub(crate) ping_err: Option<Error>,
    /// Returned by `events` instead of a stream: a connect that fails.
    pub(crate) events_err: Option<Error>,
    /// The `events` calls, numbered from 1, that never answer.
    pub(crate) events_block: BTreeSet<usize>,
    /// Runs inside `container_list` once it answers, as the enumeration runs.
    pub(crate) on_list: Option<Box<dyn Fn() + Send + Sync>>,
    /// Stamps each list, when set.
    pub(crate) clock: Option<Arc<dyn Clock>>,

    // What the fake records, public only so a test can build it with `..FakeDocker::default()`;
    // read through the methods.
    pub(crate) first_events: Mutex<Option<mpsc::UnboundedReceiver<FakeEvent>>>,
    pub(crate) calls: Mutex<Vec<Call>>,
    pub(crate) list_times: Mutex<Vec<Timestamp>>,
    pub(crate) inspects_ended: Mutex<Vec<ContextError>>,
    pub(crate) events_returned: Arc<AtomicUsize>,
    pub(crate) open_streams: Arc<AtomicUsize>,
}

impl FakeDocker {
    /// A fake that lists the containers `ids`.
    pub(crate) fn listing(ids: &[&str]) -> Self {
        Self {
            containers: ids
                .iter()
                .map(|id| ContainerSummary {
                    id: (*id).to_owned(),
                })
                .collect(),
            ..Self::default()
        }
    }

    /// The sender of the events the first stream yields; dropping it ends that stream as a
    /// closed channel.
    pub(crate) fn first_stream(&mut self) -> mpsc::UnboundedSender<FakeEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.first_events = Mutex::new(Some(receiver));
        sender
    }

    /// Every call so far, in order.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// How many times `container_list` was called.
    pub(crate) fn list_calls(&self) -> usize {
        self.count(|call| *call == Call::List)
    }

    /// The `since` of every `events` call.
    pub(crate) fn sinces(&self) -> Vec<Timestamp> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Events(since) => Some(since),
                Call::Ping | Call::List | Call::Inspect(_) => None,
            })
            .collect()
    }

    /// The containers inspected, in order.
    pub(crate) fn inspected(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Inspect(id) => Some(id),
                Call::Ping | Call::List | Call::Events(_) => None,
            })
            .collect()
    }

    /// When each list was made, by [`FakeDocker::clock`].
    pub(crate) fn list_times(&self) -> Vec<Timestamp> {
        self.list_times.lock().unwrap().clone()
    }

    /// How each blocked inspect's context ended.
    pub(crate) fn inspects_ended(&self) -> Vec<ContextError> {
        self.inspects_ended.lock().unwrap().clone()
    }

    /// How many `events` calls started, and how many have returned or been dropped: Go's check
    /// that no subscription goroutine leaked.
    pub(crate) fn events_calls(&self) -> (usize, usize) {
        let started = self.count(|call| matches!(call, Call::Events(_)));
        (started, self.events_returned.load(Ordering::SeqCst))
    }

    /// How many streams the fake handed out are still held.
    pub(crate) fn open_streams(&self) -> usize {
        self.open_streams.load(Ordering::SeqCst)
    }

    fn count(&self, matches: impl Fn(&Call) -> bool) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| matches(call))
            .count()
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}

/// Counts a call as returned when its future ends, answered or dropped.
struct Returned(Arc<AtomicUsize>);

impl Drop for Returned {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl DockerApi for FakeDocker {
    type Events = FakeEvents;

    fn ping(&self) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(Call::Ping);
        std::future::ready(self.ping_err.clone().map_or(Ok(()), Err))
    }

    fn container_list(&self) -> impl Future<Output = Result<Vec<ContainerSummary>, Error>> + Send {
        self.record(Call::List);
        if let Some(clock) = &self.clock {
            self.list_times.lock().unwrap().push(clock.now());
        }
        let failing = self
            .list_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        let reply = if failing {
            Err(Error::connection_failed("list failed".to_owned()))
        } else {
            if !self.list_blocks
                && let Some(on_list) = &self.on_list
            {
                on_list();
            }
            Ok(self.containers.clone())
        };
        let blocks = self.list_blocks;
        async move {
            if blocks {
                pending::<()>().await;
            }
            reply
        }
    }

    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, Error>> + Send {
        self.record(Call::Inspect(id.to_owned()));
        let reply = Ok(self.inspects.get(id).cloned().unwrap_or_default());
        let blocks = self.inspect_blocks;
        let ended = &self.inspects_ended;
        async move {
            if !blocks {
                return reply;
            }
            // The caller's context, which the client would race the request against.
            let Some(call) = context::current() else {
                return pending().await;
            };
            let why = call.done().await;
            ended.lock().unwrap().push(why);
            Err(Error::context(why))
        }
    }

    fn events(
        &self,
        since: Timestamp,
        _filters: &Filters,
    ) -> impl Future<Output = Result<FakeEvents, Error>> + Send {
        self.record(Call::Events(since));
        let number = self.count(|call| matches!(call, Call::Events(_)));
        let returned = Returned(Arc::clone(&self.events_returned));
        let blocks = self.events_block.contains(&number);
        let reply = match &self.events_err {
            Some(error) => Err(error.clone()),
            None => Ok(FakeEvents::new(
                if number == 1 {
                    self.first_events.lock().unwrap().take()
                } else {
                    None
                },
                Arc::clone(&self.open_streams),
            )),
        };
        async move {
            let _returned = returned;
            if blocks {
                pending::<()>().await;
            }
            reply
        }
    }
}

/// A stream of the fake's events: the test's channel, or none, which stays open and silent.
#[derive(Debug)]
pub(crate) struct FakeEvents {
    events: Option<mpsc::UnboundedReceiver<FakeEvent>>,
    open: Arc<AtomicUsize>,
}

impl FakeEvents {
    /// A stream yielding what is sent on `events`, ended as a closed channel once the sender is
    /// dropped; without one, it never yields.
    pub(crate) fn new(
        events: Option<mpsc::UnboundedReceiver<FakeEvent>>,
        open: Arc<AtomicUsize>,
    ) -> Self {
        open.fetch_add(1, Ordering::SeqCst);
        Self { events, open }
    }

    /// A stream of what is sent on the returned sender, outside any fake.
    pub(crate) fn channel() -> (mpsc::UnboundedSender<FakeEvent>, Self) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (sender, Self::new(Some(receiver), Arc::default()))
    }
}

impl Drop for FakeEvents {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl EventStream for FakeEvents {
    async fn next(&mut self) -> Result<Event, StreamError> {
        let Some(events) = &mut self.events else {
            return pending().await;
        };
        events.recv().await.unwrap_or_else(|| {
            Err(StreamError::new(
                StreamErrorKind::Closed,
                "events stream closed",
            ))
        })
    }
}
