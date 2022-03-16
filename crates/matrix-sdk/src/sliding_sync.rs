// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::BTreeMap,
    fmt::{self, Debug},
    future::Future,
    io::Read,
    pin::Pin,
    sync::Arc,
};

use anyhow::Context;
use anymap2::any::CloneAnySendSync;
use dashmap::DashMap;
use futures_core::stream::Stream;
use futures_util::{pin_mut, stream::StreamExt};
use matrix_sdk_base::{
    deserialized_responses::SyncResponse,
    media::{MediaEventContent, MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
    BaseClient, Session, Store,
};
use matrix_sdk_common::locks::RwLock;
// use core::pin::Pin;
// use futures::{
//     stream::Stream,
//     task::Poll
// };
use ruma::api::client::sync::syncv3_events;
use ruma::{assign, events::EventType, serde::Raw, RoomId, UInt};
use tracing::{error, info, instrument, warn};
use url::Url;

use crate::Client;
/// Define the state the SlidingSync View is in
///
/// The lifetime of a SlidingSync usually starts at a `Preload`, getting a fast
/// response for the first given number of Rooms, then switches into
/// `CatchingUp` during which the view fetches the remaining rooms, usually in
/// order, some times in batches. Once that is ready, it switches into `Live`.
///
/// If the client has been offline for a while, though, the SlidingSync might
/// return back to `CatchingUp` at any point.
#[derive(Debug, Clone, PartialEq)]
pub enum SlidingSyncState {
    /// Hasn't started yet
    Cold,
    /// We are quickly preloading a preview of the most important rooms
    Preload,
    /// We are trying to load all remaining rooms, might be in batches
    CatchingUp,
    /// We are all caught up and now only sync the live responses.
    Live,
}

impl Default for SlidingSyncState {
    fn default() -> Self {
        SlidingSyncState::Cold
    }
}

#[allow(dead_code)]
/// Define the mode by which the the SlidingSyncView is in fetching the data
#[derive(Debug, Clone, PartialEq)]
pub enum SlidingSyncMode {
    /// FullSync all rooms in the background, configured with batch size
    FullSync,
    /// Only sync the specific windows defined
    Selective,
}

impl Default for SlidingSyncMode {
    fn default() -> Self {
        SlidingSyncMode::FullSync
    }
}

/// Room info as giving by the SlidingSync Feature
pub type SlidingSyncRoom = syncv3_events::Room;

type ViewState = futures_signals::signal::Mutable<SlidingSyncState>;
type SyncMode = futures_signals::signal::Mutable<SlidingSyncMode>;
type PosState = futures_signals::signal::Mutable<Option<String>>;
type RangeState = futures_signals::signal::Mutable<Vec<(UInt, UInt)>>;
type RoomsCount = futures_signals::signal::Mutable<Option<u32>>;
type RoomsList = Arc<futures_signals::signal_vec::MutableVec<Option<Box<RoomId>>>>;
type RoomsMap = Arc<futures_signals::signal_map::MutableBTreeMap<Box<RoomId>, SlidingSyncRoom>>;
type ViewsList = Arc<futures_signals::signal_vec::MutableVec<SlidingSyncView>>;
pub type Cancel = futures_signals::signal::Mutable<bool>;

use derive_builder::Builder;

#[derive(Clone, Debug, Builder)]
pub struct SlidingSync {
    #[builder(setter(strip_option))]
    homeserver: Option<Url>,

    #[builder(private)]
    client: Client,
    // ------ Inernal state
    #[builder(private, default)]
    pos: PosState,
    #[builder(private, default)]
    pub views: ViewsList,
}

impl SlidingSyncBuilder {

    /// Convenience function to add a full-sync view to the builder
    pub fn add_fullsync_view(&mut self) -> &mut Self {
        let mut new = self;
        let mut views = new.views.clone().unwrap_or_default();
        views.lock_mut().push_cloned(
            SlidingSyncViewBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync view doesn't fail"),
        );
        new.views = Some(views);
        new
    }

    /// Reset the views to None
    pub fn no_views(&mut self) -> &mut Self {
        let mut new = self;
        new.views = None;
        new
    }

    /// Add the given view to the list of views
    pub fn add_view(&mut self, v: SlidingSyncView) -> &mut Self {
        let mut new = self;
        let mut views = new.views.clone().unwrap_or_default();
        views.lock_mut().push_cloned(v);
        new.views = Some(views);
        new
    }
}

impl SlidingSync {
    /// Generate a new SlidingSyncBuilder with the same inner settings and views
    /// but without the current state
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let mut builder = SlidingSyncBuilder::default()
            .client(self.client.clone())
            .views(Arc::new(futures_signals::signal_vec::MutableVec::new_with_values(
                self.views
                    .lock_ref()
                    .to_vec()
                    .iter()
                    .map(|v| {
                        v.new_builder().build().expect("builder worked before, builder works now")
                    })
                    .collect(),
            )))
            .to_owned();

        if let Some(ref h) = self.homeserver {
            builder.homeserver(h.clone());
        }
        builder
    }

    fn handle_response(
        &self,
        resp: syncv3_events::Response,
        views: &[SlidingSyncView],
    ) -> anyhow::Result<()> {
        self.pos.replace(Some(resp.pos));

        if let Some(ops) = resp.ops {
            let mut mapped_ops = Vec::new();
            mapped_ops.resize_with(views.len(), Vec::new);

            for (idx, ops) in ops
                .iter()
                .filter_map(|r| r.deserialize().ok())
                .fold(mapped_ops, |mut mp, op| {
                    let idx: u32 =
                        op.list.try_into().expect("the list index is convertible into u32");
                    mp[idx as usize].push(op);
                    mp
                })
                .iter()
                .enumerate()
            {
                let count: u32 = resp.counts[idx].try_into().context("conversion always works")?;
                views[idx].handle_response(count, ops)?;
            }
        }
        Ok(())
    }

    /// Create the inner stream for the view
    pub fn stream<'a>(
        &'a self,
    ) -> anyhow::Result<(Cancel, impl Stream<Item = anyhow::Result<()>> + 'a)> {
        let views = self.views.lock_ref().to_vec();
        let cancel = Cancel::new(false);
        let ret_cancel = cancel.clone();
        let pos = self.pos.clone();

        // FIXME: hack for while the sliding sync server is on a proxy
        let mut inner_client = self.client.inner.http_client.clone();
        let server_versions = self.client.inner.server_versions.clone();
        if let Some(hs) = &self.homeserver {
            inner_client.homeserver = Arc::new(RwLock::new(hs.clone()))
        }

        let final_stream = async_stream::try_stream! {
            let mut remaining_views = views.clone();
            let mut remaining_generators: Vec<SlidingSyncViewRequestGenerator<'_>> = views
                .iter()
                .map(SlidingSyncView::request_generator)
                .collect();
            loop {
                let requests;
                (requests, remaining_generators, remaining_views) = remaining_generators
                    .into_iter()
                    .zip(remaining_views)
                    .fold(
                    (Vec::new(), Vec::new(), Vec::new()), |mut c, (mut g, v)| {
                        if let Some(r) = g.next() {
                            c.0.push(r);
                            c.1.push(g);
                            c.2.push(v);
                        }
                        c
                    });

                if remaining_views.is_empty() {
                    return
                }
                let pos = self.pos.get_cloned();
                let mut req = assign!(syncv3_events::Request::new(), {
                    pos: pos.as_deref(),
                });
                req.body.lists = requests;
                if cancel.get() {
                    return
                }
                warn!("requesting: {:#?}", req);
                let resp = inner_client.send(req, None, server_versions.clone()).await?;
                if cancel.get() {
                    return
                }

                self.handle_response(resp, &remaining_views)?;
                yield
            }
        };

        Ok((ret_cancel, final_stream))
    }
}

/// Holding a specific filtered view within the concept of sliding sync
#[derive(Clone, Debug, Builder)]
pub struct SlidingSyncView {
    #[allow(dead_code)]
    #[builder(setter(strip_option), default)]
    sync_mode: SyncMode,

    #[builder(default = "self.default_sort()")]
    sort: Vec<String>,

    #[builder(default = "self.default_required_state()")]
    required_state: Vec<(EventType, String)>,

    #[builder(default = "20")]
    batch_size: u32,

    // ----- Public state
    /// The state this view is in
    #[builder(default)]
    pub state: ViewState,
    /// The total known number of rooms,
    #[builder(default)]
    pub rooms_count: RoomsCount,
    /// The rooms in order
    #[builder(default)]
    pub rooms_list: RoomsList,
    /// The rooms details
    #[builder(default)]
    pub rooms: RoomsMap,

    #[builder(setter(name = "ranges_raw"), default)]
    ranges: RangeState,
}

impl SlidingSyncViewBuilder {

    /// Create a Builder set up for full sync
    pub fn default_with_fullsync() -> Self {
        Self::default()
            .sync_mode(SyncMode::new(SlidingSyncMode::FullSync))
            .to_owned()
    }

    // defaults
    fn default_sort(&self) -> Vec<String> {
        vec!["by_recency".to_string(), "by_name".to_string()]
    }

    fn default_required_state(&self) -> Vec<(EventType, String)> {
        vec![
            (EventType::RoomAvatar, "".to_string()),
            (EventType::RoomMember, "*".to_string()),
            (EventType::RoomEncryption, "".to_string()),
            (EventType::RoomTombstone, "".to_string()),
        ]
    }

    /// Set the ranges to fetch
    pub fn ranges<U: Into<UInt>>(&mut self, range: Vec<(U, U)>) -> &mut Self {
        let mut new = self;
        new.ranges =
            Some(RangeState::new(range.into_iter().map(|(a, b)| (a.into(), b.into())).collect()));
        new
    }
}

enum InnerSlidingSyncViewRequestGenerator {
    FullSync(u32, u32), // current position, batch_size
    Live,
}

struct SlidingSyncViewRequestGenerator<'a> {
    view: &'a SlidingSyncView,
    inner: InnerSlidingSyncViewRequestGenerator,
}

impl<'a> SlidingSyncViewRequestGenerator<'a> {
    fn new_with_syncup(view: &'a SlidingSyncView) -> Self {
        let batch_size = view.batch_size.clone();

        SlidingSyncViewRequestGenerator {
            view,
            inner: InnerSlidingSyncViewRequestGenerator::FullSync(0, batch_size),
        }
    }

    fn new_live(view: &'a SlidingSyncView) -> Self {
        SlidingSyncViewRequestGenerator { view, inner: InnerSlidingSyncViewRequestGenerator::Live }
    }

    fn prefetch_request(
        &self,
        start: u32,
        batch_size: u32,
    ) -> (u32, Raw<syncv3_events::SyncRequestList>) {
        let end = start + batch_size;
        let ranges = vec![(start.into(), end.into())];
        (end, self.make_request_for_ranges(ranges))
    }

    fn make_request_for_ranges(
        &self,
        ranges: Vec<(UInt, UInt)>,
    ) -> Raw<syncv3_events::SyncRequestList> {
        let sort = Some(self.view.sort.clone());
        let required_state = Some(self.view.required_state.clone());
        let timeline_limit = None;
        let filters = None;

        Raw::new(&assign!(syncv3_events::SyncRequestList::default(), {
            ranges,
            required_state,
            sort,
            timeline_limit,
            filters,
        }))
        .expect("Generting request data doesn't fail")
    }

    // generate the next live request
    fn live_request(&self) -> Raw<syncv3_events::SyncRequestList> {
        let ranges = self.view.ranges.read_only().get_cloned();
        self.make_request_for_ranges(ranges)
    }
}

impl<'a> core::iter::Iterator for SlidingSyncViewRequestGenerator<'a> {
    type Item = Raw<syncv3_events::SyncRequestList>;
    fn next(&mut self) -> Option<Self::Item> {
        if let InnerSlidingSyncViewRequestGenerator::FullSync(cur_pos, _) = self.inner {
            if let Some(count) = self.view.rooms_count.get_cloned() {
                if count <= cur_pos {
                    // we are switching to live mode
                    self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                        *before == SlidingSyncState::CatchingUp
                    });
                    self.inner = InnerSlidingSyncViewRequestGenerator::Live
                }
            } else {
                // upon first catch up request, we want to switch state
                self.view.state.set_if(SlidingSyncState::Preload, |before, _now| {
                    *before == SlidingSyncState::Cold
                });
            }
        }
        match self.inner {
            InnerSlidingSyncViewRequestGenerator::FullSync(cur_pos, batch_size) => {
                let (end, req) = self.prefetch_request(cur_pos, batch_size);
                self.inner = InnerSlidingSyncViewRequestGenerator::FullSync(end, batch_size);
                self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                    *before == SlidingSyncState::Preload
                });
                Some(req)
            }
            InnerSlidingSyncViewRequestGenerator::Live => Some(self.live_request()),
        }
    }
}

impl SlidingSyncView {
    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncViewBuilder {
        SlidingSyncViewBuilder::default()
            .sync_mode(self.sync_mode.clone())
            .sort(self.sort.clone())
            .required_state(self.required_state.clone())
            .batch_size(self.batch_size)
            .ranges(self.ranges.read_only().get_cloned())
            .to_owned()
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges(&mut self, range: Vec<(u32, u32)>) -> &mut Self {
        *self.ranges.lock_mut() = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&mut self, start: u32, end: u32) {
        self.ranges.lock_mut().push((start.into(), end.into()));
    }

    /// Return the subset of rooms, starting at offset (default 0) returning
    /// count (or to the end) items
    pub fn get_rooms(
        &self,
        offset: Option<usize>,
        count: Option<usize>,
    ) -> Vec<syncv3_events::Room> {
        let start = offset.unwrap_or(0);
        let rooms = self.rooms.lock_ref();
        let listing = self.rooms_list.lock_ref();
        let count = count.unwrap_or_else(|| listing.len() - start);
        listing
            .iter()
            .skip(start)
            .filter_map(|id| id.as_ref())
            .filter_map(|id| rooms.get(id))
            .take(count)
            .cloned()
            .collect()
    }

    fn room_ops(&self, ops: &Vec<syncv3_events::SyncOp>) -> anyhow::Result<()> {
        let mut rooms_list = self.rooms_list.lock_mut();
        let mut rooms_map = self.rooms.lock_mut();
        for op in ops {
            let mut room_ids = Vec::new();
            {
                for room in &op.rooms {
                    let r: Box<RoomId> =
                        room.room_id.clone().context("Sliding Sync without RoomdId")?.parse()?;
                    rooms_map.insert_cloned(r.clone(), room.clone());
                    room_ids.push(r);
                }
            }

            match op.op {
                syncv3_events::SlidingOp::Sync => {
                    let start: u32 = op.range.0.try_into()?;
                    room_ids
                        .into_iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let idx = start as usize + i;
                            rooms_list.set_cloned(idx, Some(r));
                        })
                        .count();
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_response(
        &self,
        rooms_count: u32,
        ops: &Vec<syncv3_events::SyncOp>,
    ) -> anyhow::Result<()> {
        let mut missing =
            rooms_count.checked_sub(self.rooms_list.lock_ref().len() as u32).unwrap_or_default();
        if missing > 0 {
            let mut list = self.rooms_list.lock_mut();
            list.reserve_exact(missing as usize);
            while missing > 0 {
                list.push_cloned(None);
                missing -= 1;
            }
            self.rooms_count.replace(Some(rooms_count));
        }

        if !ops.is_empty() {
            self.room_ops(ops)?;
        }

        Ok(())
    }

    fn request_generator<'a>(&'a self) -> SlidingSyncViewRequestGenerator<'a> {
        match self.sync_mode.read_only().get_cloned() {
            SlidingSyncMode::FullSync => SlidingSyncViewRequestGenerator::new_with_syncup(self),
            SlidingSyncMode::Selective => SlidingSyncViewRequestGenerator::new_live(self),
        }
    }
}

impl Client {
    /// Create a SlidingSyncBuilder tied to this client
    pub fn sliding_sync(&self) -> SlidingSyncBuilder {
        SlidingSyncBuilder::default().client(self.clone()).to_owned()
    }
}