//! Watch and cache the remote Kubernetes API resources.

use super::{
    resource_version,
    watcher::{self, Watcher},
};
use futures::{
    pin_mut,
    stream::{Stream, StreamExt},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, WatchEvent},
    Metadata, WatchOptional, WatchResponse,
};
use snafu::Snafu;
use std::convert::Infallible;
use std::time::Duration;
use tokio::{select, time::delay_for};

use super::{delayed_delete::DelayedDelete, state};

/// Watches remote Kubernetes resources and maintains a local representation of
/// the remote state. "Reflects" the remote state locally.
///
/// Does not expose evented API, but keeps track of the resource versions and
/// will automatically resume on desync.
pub struct Reflector<W, S>
where
    W: Watcher,
    <W as Watcher>::Object: Metadata<Ty = ObjectMeta>,
    S: state::Write<Item = <W as Watcher>::Object>,
{
    watcher: W,
    state_writer: S,
    field_selector: Option<String>,
    label_selector: Option<String>,
    resource_version: resource_version::State,
    pause_between_requests: Duration,
    delayed_delete: Option<DelayedDelete<<W as Watcher>::Object>>,
}

impl<W, S> Reflector<W, S>
where
    W: Watcher,
    <W as Watcher>::Object: Metadata<Ty = ObjectMeta>,
    S: state::Write<Item = <W as Watcher>::Object>,
{
    /// Create a new [`Cache`].
    pub fn new(
        watcher: W,
        state_writer: S,
        field_selector: Option<String>,
        label_selector: Option<String>,
        pause_between_requests: Duration,
        delay_deletes_for: Option<Duration>,
    ) -> Self {
        let resource_version = resource_version::State::new();
        let delayed_delete = delay_deletes_for.map(DelayedDelete::new);
        Self {
            watcher,
            state_writer,
            label_selector,
            field_selector,
            resource_version,
            pause_between_requests,
            delayed_delete,
        }
    }
}

impl<W, S> Reflector<W, S>
where
    W: Watcher,
    <W as Watcher>::Object: Metadata<Ty = ObjectMeta> + Unpin + std::fmt::Debug,
    <W as Watcher>::InvocationError: Unpin,
    <W as Watcher>::StreamError: Unpin,
    S: state::Write<Item = <W as Watcher>::Object>,
{
    /// Run the watch loop and drive the state updates via `state_writer`.
    pub async fn run(
        &mut self,
    ) -> Result<Infallible, Error<<W as Watcher>::InvocationError, <W as Watcher>::StreamError>>
    {
        // Start the watch loop.
        loop {
            let invocation_result = self.issue_request().await;
            let stream = match invocation_result {
                Ok(val) => val,
                Err(watcher::invocation::Error::Desync { source }) => {
                    warn!(message = "handling desync", error = ?source);
                    // We got desynced, reset the state and retry fetching.
                    // By omiting the flush here, we cache the results from the
                    // previous run until flush is issued when the new events
                    // begin arriving, reducing the time durig which the state
                    // has no data.
                    self.resource_version.reset();
                    if let Some(ref mut delayed_delete) = self.delayed_delete {
                        delayed_delete.clear();
                    }
                    self.state_writer.resync();
                    continue;
                }
                Err(watcher::invocation::Error::Other { source }) => {
                    // Not a desync, fail everything.
                    error!(message = "watcher error", error = ?source);
                    return Err(Error::Invocation { source });
                }
            };

            pin_mut!(stream);
            loop {
                // Obtain an value from the watch stream.
                let val = if let Some(ref mut delayed_delete) = self.delayed_delete {
                    // If delayed delete is requested, we perform the delayed
                    // deletions concurrently to reading items from the watch
                    // responses stream.
                    let (delayed_delete_delay, should_poll_delayed_delete_delay) =
                        delayed_delete.next_deadline_delay();
                    select! {
                        // If we get a delayted delete deadline - process the
                        // delayed deletes and restart the loop.
                        _ = delayed_delete_delay, if should_poll_delayed_delete_delay => {
                            delayed_delete.perform(&mut self.state_writer);
                            continue;
                        }
                        // If we got a value from the watch responses stream -
                        // just pass it outside.
                        val = stream.next() => val,
                    }
                } else {
                    // Delayed deletes aren't requested, so just wait for the
                    // next value and pass it outside.
                    stream.next().await
                };

                if let Some(item) = val {
                    // A new item arrived from the watch response stream
                    // first - process it.
                    self.process_stream_item(item)?;
                } else {
                    // Response stream has ended.
                    // Break the watch reading loop so the flow can
                    // continue an issue a new watch request.
                    break;
                }
            }

            // For the next pause duration we won't get any updates.
            // This is better than flooding k8s api server with requests.
            delay_for(self.pause_between_requests).await;
        }
    }

    /// Prepare and execute a watch request.
    async fn issue_request(
        &mut self,
    ) -> Result<<W as Watcher>::Stream, watcher::invocation::Error<<W as Watcher>::InvocationError>>
    {
        let watch_optional = WatchOptional {
            field_selector: self.field_selector.as_ref().map(|s| s.as_str()),
            label_selector: self.label_selector.as_ref().map(|s| s.as_str()),
            pretty: None,
            resource_version: self.resource_version.get(),
            timeout_seconds: None,
            allow_watch_bookmarks: Some(true),
        };
        let stream = self.watcher.watch(watch_optional).await?;
        Ok(stream)
    }

    /// Process an item from the watch response stream.
    fn process_stream_item(
        &mut self,
        item: <<W as Watcher>::Stream as Stream>::Item,
    ) -> Result<(), Error<<W as Watcher>::InvocationError, <W as Watcher>::StreamError>> {
        // Any streaming error means the protocol is in an unxpected
        // state. This is considered a fatal error, do not attempt
        // to retry and just quit.
        let response = item.map_err(|source| Error::Streaming { source })?;

        // Unpack the event.
        let event = match response {
            WatchResponse::Ok(event) => event,
            WatchResponse::Other(_) => {
                // Even though we could parse the response, we didn't
                // get the data we expected on the wire.
                // According to the rules, we just ignore the unknown
                // responses. This may be a newly added piece of data
                // our code doesn't know of.
                // TODO: add more details on the data here if we
                // encounter these messages in practice.
                warn!(message = "got unexpected data in the watch response");
                return Ok(());
            }
        };

        // Prepare a resource version candidate so we can update (aka commit) it
        // later.
        let resource_version_candidate = match resource_version::Candidate::from_watch_event(&event)
        {
            Some(val) => val,
            None => {
                // This event doesn't have a resource version, this means
                // it's not something we care about.
                return Ok(());
            }
        };

        // Process the event.
        self.process_event(event);

        // Record the resourse version for this event, so when we resume
        // it won't be redelivered.
        self.resource_version.update(resource_version_candidate);

        Ok(())
    }

    /// Translate received watch event to the state update.
    fn process_event(&mut self, event: WatchEvent<<W as Watcher>::Object>) {
        match event {
            WatchEvent::Added(object) => {
                trace!(message = "got an object event", event = "added");
                self.state_writer.add(object);
            }
            WatchEvent::Deleted(object) => {
                trace!(message = "got an object event", event = "deleted");
                if let Some(ref mut delayed_delete) = self.delayed_delete {
                    delayed_delete.schedule_delete(object);
                } else {
                    self.state_writer.delete(object);
                }
            }
            WatchEvent::Modified(object) => {
                trace!(message = "got an object event", event = "modified");
                self.state_writer.update(object);
            }
            WatchEvent::Bookmark(_object) => {
                trace!(message = "got an object event", event = "bookmark");
                // noop
            }
            _ => unreachable!("other event types should never reach this code"),
        }
    }
}

/// Errors that can occur while watching.
#[derive(Debug, Snafu)]
pub enum Error<I, S>
where
    I: std::error::Error + 'static,
    S: std::error::Error + 'static,
{
    /// Returned when the watch invocation (HTTP request) failed.
    #[snafu(display("watch invocation failed"))]
    Invocation {
        /// The underlying invocation error.
        source: I,
    },

    /// Returned when the stream failed with an error.
    #[snafu(display("streaming error"))]
    Streaming {
        /// The underlying stream error.
        source: S,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        state::evmap::{Value, Writer},
        Reflector,
    };
    use crate::{
        kubernetes::mock_watcher::{InvocationError, MockWatcher},
        kubernetes::watcher,
        test_util,
    };
    use k8s_openapi::{
        api::core::v1::Pod,
        apimachinery::pkg::apis::meta::v1::{ObjectMeta, WatchEvent},
        Metadata, WatchOptional, WatchResponse,
    };
    use std::time::Duration;

    /// A helper function to simplify assertion on the `evmap` state.
    fn gather_state<T>(handle: &evmap10::ReadHandle<String, Value<T>>) -> Vec<T>
    where
        T: Metadata<Ty = ObjectMeta> + Clone,
    {
        let mut vec: Vec<(String, T)> = handle
            .read()
            .expect("expected read to be ready")
            .iter()
            .map(|(key, values)| {
                assert_eq!(values.len(), 1);
                let value = values.get_one().unwrap();
                (key.clone(), value.as_ref().as_ref().to_owned())
            })
            .collect();

        // Sort the results by key for consistent assertions.
        vec.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        // Discard keys.
        vec.into_iter().map(|(_, value)| value).collect()
    }

    // A helper to build a pod object for test purposes.
    fn make_pod(uid: &str, resource_version: &str) -> Pod {
        Pod {
            metadata: Some(ObjectMeta {
                uid: Some(uid.to_owned()),
                resource_version: Some(resource_version.to_owned()),
                ..ObjectMeta::default()
            }),
            ..Pod::default()
        }
    }

    // A helper to build a bookmark pod object.
    // See https://github.com/kubernetes/enhancements/blob/e565a82680bb8e05836530ebd1abac723aab40e2/keps/sig-api-machinery/20190206-watch-bookmark.md
    fn make_pod_bookmark(resource_version: &str) -> Pod {
        Pod {
            metadata: Some(ObjectMeta {
                resource_version: Some(resource_version.to_owned()),
                ..ObjectMeta::default()
            }),
            ..Pod::default()
        }
    }

    // A type alias to add expressiveness.
    type StateSnapshot = Vec<Pod>;

    // A helper enum to encode expected mock watcher invocation.
    enum ExpInvRes {
        Stream(Vec<WatchEvent<Pod>>),
        Desync,
    }

    // A simple test, to serve as a bare-bones example for adding further tests.
    #[test]
    fn simple_test() {
        test_util::trace_init();

        // Prepare the test flow.
        let (_state_reader, state_writer) = evmap10::new();
        let state_writer = Writer::new(state_writer);
        let mock_logic = move |_watch_optional: WatchOptional<'_>| {
            if false {
                return Ok(|| None); // for type inferrence
            }
            Err(watcher::invocation::Error::other(InvocationError))
        };

        // Prepare watcher and reflector.
        let watcher: MockWatcher<Pod, _> = MockWatcher::new(mock_logic);
        let mut reflector = Reflector::new(
            watcher,
            state_writer,
            None,
            None,
            Duration::from_secs(1),
            None,
        );

        // Acquire an async context to run the relector.
        test_util::block_on_std(async move {
            // Run the test and wait for an error.
            let result = reflector.run().await;
            // The only way reflector completes is with an error, but that's ok.
            // In tests we make it exit with an error to complete the test.
            result.unwrap_err();

            // Explicitly drop the reflector at the very end.
            // Internal evmap is dropped with the reflector, so readers won't
            // work after drop.
            drop(reflector);
        });
    }

    // A helper function to run a flow test.
    // Use this to test various flows without an actual test repetition.
    fn run_flow_test(
        invocations: Vec<(StateSnapshot, Option<String>, ExpInvRes)>,
        expected_resulting_state: StateSnapshot,
    ) {
        // Prepare the test flow.
        let (state_reader, state_writer) = evmap10::new();
        let state_writer = Writer::new(state_writer);

        let assertion_state_reader = state_reader.clone();
        let flow_expected_resulting_state = expected_resulting_state.clone();

        let mut flow_invocations = invocations.into_iter();

        let mock_logic = move |watch_optional: WatchOptional<'_>| {
            if let Some((
                expected_state_before_op,
                expected_resource_version,
                expected_invocation_response,
            )) = flow_invocations.next()
            {
                // Assert the state prior to the operation.
                let state: StateSnapshot = gather_state(&assertion_state_reader);
                assert_eq!(state, expected_state_before_op);

                assert_eq!(
                    expected_resource_version,
                    watch_optional.resource_version.map(ToOwned::to_owned) // work around the borrow checker issues
                );

                let responses = match expected_invocation_response {
                    ExpInvRes::Stream(responses) => responses,
                    ExpInvRes::Desync => {
                        return Err(watcher::invocation::Error::desync(InvocationError))
                    }
                };

                let mut responses_iter = responses.into_iter();
                return Ok(move || responses_iter.next().map(|val| Ok(WatchResponse::Ok(val))));
            }

            let resulting_state: StateSnapshot = gather_state(&assertion_state_reader);
            assert_eq!(resulting_state, flow_expected_resulting_state);

            Err(watcher::invocation::Error::other(InvocationError))
        };

        // Prepare watcher and reflector.
        let watcher: MockWatcher<Pod, _> = MockWatcher::new(mock_logic);
        let mut reflector = Reflector::new(
            watcher,
            state_writer,
            None,
            None,
            Duration::from_secs(1),
            None,
        );

        // Acquire an async context to run the relector.
        test_util::block_on_std(async move {
            // Run the test and wait for an error.
            let result = reflector.run().await;
            // The only way reflector completes is with an error, but that's ok.
            // In tests we make it exit with an error to complete the test.
            result.unwrap_err();

            // Assert the state after the reflector exit.
            let resulting_state: StateSnapshot = gather_state(&state_reader);
            assert_eq!(resulting_state, expected_resulting_state);

            // Explicitly drop the reflector at the very end.
            // Internal evmap is dropped with the reflector, so readers won't
            // work after drop.
            drop(reflector);
        });
    }

    // Test the properties of the normal  execution flow.
    #[test]
    fn flow_test() {
        test_util::trace_init();

        let invocations = vec![
            (
                vec![],
                None,
                ExpInvRes::Stream(vec![
                    WatchEvent::Added(make_pod("uid0", "10")),
                    WatchEvent::Added(make_pod("uid1", "15")),
                ]),
            ),
            (
                vec![make_pod("uid0", "10"), make_pod("uid1", "15")],
                Some("15".to_owned()),
                ExpInvRes::Stream(vec![
                    WatchEvent::Modified(make_pod("uid0", "20")),
                    WatchEvent::Added(make_pod("uid2", "25")),
                ]),
            ),
            (
                vec![
                    make_pod("uid0", "20"),
                    make_pod("uid1", "15"),
                    make_pod("uid2", "25"),
                ],
                Some("25".to_owned()),
                ExpInvRes::Stream(vec![WatchEvent::Bookmark(make_pod_bookmark("50"))]),
            ),
            (
                vec![
                    make_pod("uid0", "20"),
                    make_pod("uid1", "15"),
                    make_pod("uid2", "25"),
                ],
                Some("50".to_owned()),
                ExpInvRes::Stream(vec![
                    WatchEvent::Deleted(make_pod("uid2", "55")),
                    WatchEvent::Modified(make_pod("uid0", "60")),
                ]),
            ),
        ];
        let expected_resulting_state = vec![make_pod("uid0", "60"), make_pod("uid1", "15")];

        // Use standard flow test logic.
        run_flow_test(invocations, expected_resulting_state);
    }

    // Test the properies of the flow with desync.
    #[test]
    fn desync_test() {
        test_util::trace_init();

        let invocations = vec![
            (
                vec![],
                None,
                ExpInvRes::Stream(vec![
                    WatchEvent::Added(make_pod("uid0", "10")),
                    WatchEvent::Added(make_pod("uid1", "15")),
                ]),
            ),
            (
                vec![make_pod("uid0", "10"), make_pod("uid1", "15")],
                Some("15".to_owned()),
                ExpInvRes::Desync,
            ),
            (
                vec![make_pod("uid0", "10"), make_pod("uid1", "15")],
                None,
                ExpInvRes::Stream(vec![
                    WatchEvent::Added(make_pod("uid20", "1000")),
                    WatchEvent::Added(make_pod("uid21", "1005")),
                ]),
            ),
            (
                vec![make_pod("uid20", "1000"), make_pod("uid21", "1005")],
                Some("1005".to_owned()),
                ExpInvRes::Stream(vec![WatchEvent::Modified(make_pod("uid21", "1010"))]),
            ),
        ];
        let expected_resulting_state = vec![make_pod("uid20", "1000"), make_pod("uid21", "1010")];

        // Use standard flow test logic.
        run_flow_test(invocations, expected_resulting_state);
    }

    /// Test that the state is properly initialized even if no events arrived.
    #[test]
    fn no_updates_state_test() {
        test_util::trace_init();

        let invocations = vec![];
        let expected_resulting_state = vec![];

        // Use standard flow test logic.
        run_flow_test(invocations, expected_resulting_state);
    }

    // Test that [`WatchOptional`] is populated properly.
    #[test]
    fn arguments_test() {
        test_util::trace_init();

        let (_state_reader, state_writer) = evmap10::new();
        let state_writer = Writer::new(state_writer);
        let mock_logic = move |watch_optional: WatchOptional<'_>| {
            assert_eq!(watch_optional.field_selector, Some("fields"));
            assert_eq!(watch_optional.label_selector, Some("labels"));
            assert_eq!(watch_optional.allow_watch_bookmarks, Some(true));
            assert_eq!(watch_optional.pretty, None);
            assert_eq!(watch_optional.timeout_seconds, None);

            if false {
                return Ok(|| None); // for type inferrence
            }
            Err(watcher::invocation::Error::other(InvocationError))
        };

        let watcher: MockWatcher<Pod, _> = MockWatcher::new(mock_logic);

        let mut reflector = Reflector::new(
            watcher,
            state_writer,
            Some("fields".to_owned()),
            Some("labels".to_owned()),
            Duration::from_secs(1),
            Some(Duration::from_secs(60)),
        );

        // Acquire an async context to run the relector.
        test_util::block_on_std(async move {
            // Run the test and wait for an error.
            let result = reflector.run().await;
            // The only way reflector completes is with an error, but that's ok.
            // In tests we make it exit with an error to complete the test.
            result.unwrap_err();

            // Explicitly drop the reflector at the very end.
            // Internal evmap is dropped with the reflector, so readers won't
            // work after drop.
            drop(reflector);
        });
    }

    // A helper function to run a flow test.
    // Use this to test various flows without an actual test repetition.
    fn run_flow_test2(
        invocations: Vec<(StateSnapshot, Option<String>, ExpInvRes)>,
        expected_resulting_state: StateSnapshot,
    ) {
        // Prepare the test flow.
        let (state_reader, state_writer) = evmap10::new();
        let state_writer = Writer::new(state_writer);

        let assertion_state_reader = state_reader.clone();
        let flow_expected_resulting_state = expected_resulting_state.clone();

        let mut flow_invocations = invocations.into_iter();

        let mock_logic = move |watch_optional: WatchOptional<'_>| {
            if let Some((
                expected_state_before_op,
                expected_resource_version,
                expected_invocation_response,
            )) = flow_invocations.next()
            {
                // Assert the state prior to the operation.
                let state: StateSnapshot = gather_state(&assertion_state_reader);
                assert_eq!(state, expected_state_before_op);

                assert_eq!(
                    expected_resource_version,
                    watch_optional.resource_version.map(ToOwned::to_owned) // work around the borrow checker issues
                );

                let responses = match expected_invocation_response {
                    ExpInvRes::Stream(responses) => responses,
                    ExpInvRes::Desync => {
                        return Err(watcher::invocation::Error::desync(InvocationError))
                    }
                };

                let mut responses_iter = responses.into_iter();
                return Ok(move || responses_iter.next().map(|val| Ok(WatchResponse::Ok(val))));
            }

            let resulting_state: StateSnapshot = gather_state(&assertion_state_reader);
            assert_eq!(resulting_state, flow_expected_resulting_state);

            Err(watcher::invocation::Error::other(InvocationError))
        };

        // Prepare watcher and reflector.
        let watcher: MockWatcher<Pod, _> = MockWatcher::new(mock_logic);
        let mut reflector = Reflector::new(
            watcher,
            state_writer,
            None,
            None,
            Duration::from_secs(1),
            Some(Duration::from_secs(60_000)),
        );

        // Acquire an async context to run the relector.
        test_util::block_on_std(async move {
            // Run the test and wait for an error.
            let result = reflector.run().await;
            // The only way reflector completes is with an error, but that's ok.
            // In tests we make it exit with an error to complete the test.
            result.unwrap_err();

            // Assert the state after the reflector exit.
            let resulting_state: StateSnapshot = gather_state(&state_reader);
            assert_eq!(resulting_state, expected_resulting_state);

            // Explicitly drop the reflector at the very end.
            // Internal evmap is dropped with the reflector, so readers won't
            // work after drop.
            drop(reflector);
        });
    }

    // Test the properies of the flow with desync.
    #[test]
    fn qweqwe() {
        test_util::trace_init();

        let invocations = vec![
            (
                vec![],
                None,
                ExpInvRes::Stream(vec![
                    WatchEvent::Added(make_pod("uid0", "10")),
                    WatchEvent::Added(make_pod("uid1", "15")),
                ]),
            ),
            (
                vec![make_pod("uid0", "10"), make_pod("uid1", "15")],
                Some("15".to_owned()),
                ExpInvRes::Desync,
            ),
            (
                vec![make_pod("uid0", "10"), make_pod("uid1", "15")],
                None,
                ExpInvRes::Stream(vec![
                    WatchEvent::Added(make_pod("uid20", "1000")),
                    WatchEvent::Added(make_pod("uid21", "1005")),
                ]),
            ),
            (
                vec![make_pod("uid20", "1000"), make_pod("uid21", "1005")],
                Some("1005".to_owned()),
                ExpInvRes::Stream(vec![WatchEvent::Modified(make_pod("uid21", "1010"))]),
            ),
        ];
        let expected_resulting_state = vec![make_pod("uid20", "1000"), make_pod("uid21", "1010")];

        // Use standard flow test logic.
        run_flow_test2(invocations, expected_resulting_state);
    }
}