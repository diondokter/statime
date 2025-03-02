use core::ops::Deref;

use arrayvec::ArrayVec;
use atomic_refcell::{AtomicRef, AtomicRefCell};
pub use measurement::Measurement;
use rand::Rng;
use state::{MasterState, PortState};

use self::state::SlaveState;
use crate::{
    bmc::bmca::{BestAnnounceMessage, Bmca, RecommendedState},
    clock::Clock,
    config::PortConfig,
    datastructures::{
        common::{LeapIndicator, PortIdentity, TimeSource, WireTimestamp},
        datasets::{CurrentDS, DefaultDS, ParentDS, TimePropertiesDS},
        messages::{Message, MessageBody},
    },
    filters::{Filter, FilterUpdate},
    ptp_instance::PtpInstanceState,
    time::Duration,
    Time, MAX_DATA_LEN,
};

// Needs to be here because of use rules
macro_rules! actions {
    [] => {
        {
            crate::port::PortActionIterator::from(::arrayvec::ArrayVec::new())
        }
    };
    [$action:expr] => {
        {
            let mut list = ::arrayvec::ArrayVec::new();
            list.push($action);
            PortActionIterator::from(list)
        }
    };
    [$action1:expr, $action2:expr] => {
        {
            let mut list = ::arrayvec::ArrayVec::new();
            list.push($action1);
            list.push($action2);
            PortActionIterator::from(list)
        }
    };
}

mod measurement;
mod sequence_id;
pub(crate) mod state;

/// A single port of the PTP instance
///
/// One of these needs to be created per port of the PTP instance.
#[derive(Debug)]
pub struct Port<L, R, C, F: Filter> {
    config: PortConfig,
    filter_config: F::Config,
    clock: C,
    // PortDS port_identity
    pub(crate) port_identity: PortIdentity,
    // Corresponds with PortDS port_state and enabled
    port_state: PortState<F>,
    bmca: Bmca,
    packet_buffer: [u8; MAX_DATA_LEN],
    lifecycle: L,
    rng: R,
}

#[derive(Debug)]
pub struct Running<'a> {
    state_refcell: &'a AtomicRefCell<PtpInstanceState>,
    state: AtomicRef<'a, PtpInstanceState>,
}

#[derive(Debug)]
pub struct InBmca<'a> {
    pending_action: PortActionIterator<'static>,
    local_best: Option<BestAnnounceMessage>,
    state_refcell: &'a AtomicRefCell<PtpInstanceState>,
}

// Making this non-copy and non-clone ensures a single handle_send_timestamp
// per SendTimeCritical
#[derive(Debug)]
pub struct TimestampContext {
    inner: TimestampContextInner,
}

#[derive(Debug)]
enum TimestampContextInner {
    Sync { id: u16 },
    DelayReq { id: u16 },
}

#[derive(Debug)]
pub enum PortAction<'a> {
    SendTimeCritical {
        context: TimestampContext,
        data: &'a [u8],
    },
    SendGeneral {
        data: &'a [u8],
    },
    ResetAnnounceTimer {
        duration: core::time::Duration,
    },
    ResetSyncTimer {
        duration: core::time::Duration,
    },
    ResetDelayRequestTimer {
        duration: core::time::Duration,
    },
    ResetAnnounceReceiptTimer {
        duration: core::time::Duration,
    },
    ResetFilterUpdateTimer {
        duration: core::time::Duration,
    },
}

const MAX_ACTIONS: usize = 2;

/// Guarantees to end user: Any set of actions will only ever contain a single
/// time critical send
#[derive(Debug)]
#[must_use]
pub struct PortActionIterator<'a> {
    internal: <ArrayVec<PortAction<'a>, MAX_ACTIONS> as IntoIterator>::IntoIter,
}

impl<'a> PortActionIterator<'a> {
    fn from(list: ArrayVec<PortAction<'a>, MAX_ACTIONS>) -> Self {
        Self {
            internal: list.into_iter(),
        }
    }
    fn from_filter(update: FilterUpdate) -> Self {
        if let Some(duration) = update.next_update {
            actions![PortAction::ResetFilterUpdateTimer { duration }]
        } else {
            actions![]
        }
    }
}

impl<'a> Iterator for PortActionIterator<'a> {
    type Item = PortAction<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal.next()
    }
}

impl<'a, C: Clock, F: Filter, R: Rng> Port<Running<'a>, R, C, F> {
    // Send timestamp for last timecritical message became available
    pub fn handle_send_timestamp(
        &mut self,
        context: TimestampContext,
        timestamp: Time,
    ) -> PortActionIterator<'_> {
        let actions = self.port_state.handle_timestamp(
            context,
            timestamp,
            self.port_identity,
            &self.lifecycle.state.default_ds,
            &mut self.clock,
            &mut self.packet_buffer,
        );

        actions
    }

    // Handle the announce timer going of
    pub fn handle_announce_timer(&mut self) -> PortActionIterator<'_> {
        self.port_state.send_announce(
            self.lifecycle.state.deref(),
            &self.config,
            self.port_identity,
            &mut self.packet_buffer,
        )
    }

    // Handle the sync timer going of
    pub fn handle_sync_timer(&mut self) -> PortActionIterator<'_> {
        self.port_state.send_sync(
            &self.config,
            self.port_identity,
            &self.lifecycle.state.default_ds,
            &mut self.packet_buffer,
        )
    }

    // Handle the sync timer going of
    pub fn handle_delay_request_timer(&mut self) -> PortActionIterator<'_> {
        self.port_state.send_delay_request(
            &mut self.rng,
            &self.config,
            self.port_identity,
            &self.lifecycle.state.default_ds,
            &mut self.packet_buffer,
        )
    }

    // Handle the announce receipt timer going off
    pub fn handle_announce_receipt_timer(&mut self) -> PortActionIterator<'_> {
        // we didn't hear announce messages from other masters, so become master
        // ourselves
        match self.port_state {
            PortState::Master(_) => (),
            _ => self.set_forced_port_state(PortState::Master(MasterState::new())),
        }

        // Immediately start sending syncs and announces
        actions![
            PortAction::ResetAnnounceTimer {
                duration: core::time::Duration::from_secs(0)
            },
            PortAction::ResetSyncTimer {
                duration: core::time::Duration::from_secs(0)
            }
        ]
    }

    // Handle a message over the timecritical channel
    pub fn handle_timecritical_receive(
        &mut self,
        data: &[u8],
        timestamp: Time,
    ) -> PortActionIterator {
        let message = match Message::deserialize(data) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("Could not parse packet: {:?}", error);
                return actions![];
            }
        };

        // Only process messages from the same domain
        if message.header().sdo_id != self.lifecycle.state.default_ds.sdo_id
            || message.header().domain_number != self.lifecycle.state.default_ds.domain_number
        {
            return actions![];
        }

        let actions = self.port_state.handle_event_receive(
            message,
            timestamp,
            self.config.min_delay_req_interval(),
            self.port_identity,
            &mut self.clock,
            &mut self.packet_buffer,
        );

        actions
    }

    // Handle a general ptp message
    pub fn handle_general_receive(&mut self, data: &[u8]) -> PortActionIterator {
        let message = match Message::deserialize(data) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("Could not parse packet: {:?}", error);
                return actions![];
            }
        };

        // Only process messages from the same domain
        if message.header().sdo_id != self.lifecycle.state.default_ds.sdo_id
            || message.header().domain_number != self.lifecycle.state.default_ds.domain_number
        {
            return actions![];
        }

        match message.body {
            MessageBody::Announce(announce) => {
                self.bmca.register_announce_message(
                    &message.header,
                    &announce,
                    self.clock.now().into(),
                );
                actions![PortAction::ResetAnnounceReceiptTimer {
                    duration: self.config.announce_duration(&mut self.rng),
                }]
            }
            _ => {
                self.port_state
                    .handle_general_receive(message, self.port_identity, &mut self.clock)
            }
        }
    }

    pub fn handle_filter_update_timer(&mut self) -> PortActionIterator {
        self.port_state.handle_filter_update(&mut self.clock)
    }

    // Start a BMCA cycle and ensure this happens instantly from the perspective of
    // the port
    pub fn start_bmca(self) -> Port<InBmca<'a>, R, C, F> {
        Port {
            port_state: self.port_state,
            config: self.config,
            filter_config: self.filter_config,
            clock: self.clock,
            port_identity: self.port_identity,
            bmca: self.bmca,
            rng: self.rng,
            packet_buffer: [0; MAX_DATA_LEN],
            lifecycle: InBmca {
                pending_action: actions![],
                local_best: None,
                state_refcell: self.lifecycle.state_refcell,
            },
        }
    }
}

impl<'a, C, F: Filter, R> Port<InBmca<'a>, R, C, F> {
    // End a BMCA cycle and make the port available again
    pub fn end_bmca(self) -> (Port<Running<'a>, R, C, F>, PortActionIterator<'static>) {
        (
            Port {
                port_state: self.port_state,
                config: self.config,
                filter_config: self.filter_config,
                clock: self.clock,
                port_identity: self.port_identity,
                bmca: self.bmca,
                rng: self.rng,
                packet_buffer: [0; MAX_DATA_LEN],
                lifecycle: Running {
                    state_refcell: self.lifecycle.state_refcell,
                    state: self.lifecycle.state_refcell.borrow(),
                },
            },
            self.lifecycle.pending_action,
        )
    }
}

impl<L, R, C: Clock, F: Filter> Port<L, R, C, F> {
    fn set_forced_port_state(&mut self, mut state: PortState<F>) {
        log::info!(
            "new state for port {}: {} -> {}",
            self.port_identity.port_number,
            self.port_state,
            state
        );
        core::mem::swap(&mut self.port_state, &mut state);
        state.demobilize_filter(&mut self.clock);
    }
}

impl<L, R, C, F: Filter> Port<L, R, C, F> {
    pub(crate) fn state(&self) -> &PortState<F> {
        &self.port_state
    }

    pub(crate) fn number(&self) -> u16 {
        self.port_identity.port_number
    }
}

impl<'a, C: Clock, F: Filter, R: Rng> Port<InBmca<'a>, R, C, F> {
    pub(crate) fn calculate_best_local_announce_message(&mut self, current_time: WireTimestamp) {
        self.lifecycle.local_best = self.bmca.take_best_port_announce_message(current_time)
    }

    pub(crate) fn best_local_announce_message(&self) -> Option<BestAnnounceMessage> {
        // Announce messages received on a masterOnly PTP Port shall not be considered
        // in the operation of the best master clock algorithm or in the update
        // of data sets.
        if self.config.master_only {
            None
        } else {
            self.lifecycle.local_best
        }
    }

    pub(crate) fn set_recommended_state(
        &mut self,
        recommended_state: RecommendedState,
        time_properties_ds: &mut TimePropertiesDS,
        current_ds: &mut CurrentDS,
        parent_ds: &mut ParentDS,
        default_ds: &DefaultDS,
        clock: &mut C,
    ) {
        self.set_recommended_port_state(&recommended_state, default_ds);

        match recommended_state {
            RecommendedState::M1(defaultds) | RecommendedState::M2(defaultds) => {
                // a slave-only PTP port should never end up in the master state
                debug_assert!(!default_ds.slave_only);

                current_ds.steps_removed = 0;
                current_ds.offset_from_master = Duration::ZERO;
                current_ds.mean_delay = Duration::ZERO;

                parent_ds.parent_port_identity.clock_identity = defaultds.clock_identity;
                parent_ds.parent_port_identity.port_number = 0;
                parent_ds.grandmaster_identity = defaultds.clock_identity;
                parent_ds.grandmaster_clock_quality = defaultds.clock_quality;
                parent_ds.grandmaster_priority_1 = defaultds.priority_1;
                parent_ds.grandmaster_priority_2 = defaultds.priority_2;

                time_properties_ds.leap_indicator = LeapIndicator::NoLeap;
                time_properties_ds.current_utc_offset = None;
                time_properties_ds.ptp_timescale = true;
                time_properties_ds.time_traceable = false;
                time_properties_ds.frequency_traceable = false;
                time_properties_ds.time_source = TimeSource::InternalOscillator;
            }
            RecommendedState::M3(_) | RecommendedState::P1(_) | RecommendedState::P2(_) => {}
            RecommendedState::S1(announce_message) => {
                // a master-only PTP port should never end up in the slave state
                debug_assert!(!self.config.master_only);

                current_ds.steps_removed = announce_message.steps_removed + 1;

                parent_ds.parent_port_identity = announce_message.header.source_port_identity;
                parent_ds.grandmaster_identity = announce_message.grandmaster_identity;
                parent_ds.grandmaster_clock_quality = announce_message.grandmaster_clock_quality;
                parent_ds.grandmaster_priority_1 = announce_message.grandmaster_priority_1;
                parent_ds.grandmaster_priority_2 = announce_message.grandmaster_priority_2;

                *time_properties_ds = announce_message.time_properties();

                if let Err(error) = clock.set_properties(time_properties_ds) {
                    log::error!("Could not update clock: {:?}", error);
                }
            }
        }

        // TODO: Discuss if we should change the clock's own time properties, or keep
        // the master's time properties separately
        if let RecommendedState::S1(announce_message) = &recommended_state {
            // Update time properties
            *time_properties_ds = announce_message.time_properties();
        }
    }

    fn set_recommended_port_state(
        &mut self,
        recommended_state: &RecommendedState,
        default_ds: &DefaultDS,
    ) {
        match recommended_state {
            // TODO set things like steps_removed once they are added
            // TODO make sure states are complete
            RecommendedState::S1(announce_message) => {
                // a master-only PTP port should never end up in the slave state
                debug_assert!(!self.config.master_only);

                let remote_master = announce_message.header.source_port_identity;

                let update_state = match &self.port_state {
                    PortState::Listening | PortState::Master(_) | PortState::Passive => true,
                    PortState::Slave(old_state) => old_state.remote_master() != remote_master,
                };

                if update_state {
                    let state = PortState::Slave(SlaveState::new(
                        remote_master,
                        self.filter_config.clone(),
                    ));
                    self.set_forced_port_state(state);

                    let duration = self.config.announce_duration(&mut self.rng);
                    let reset_announce = PortAction::ResetAnnounceReceiptTimer { duration };
                    let reset_delay = PortAction::ResetDelayRequestTimer {
                        duration: core::time::Duration::ZERO,
                    };
                    self.lifecycle.pending_action = actions![reset_announce, reset_delay];
                }
            }
            RecommendedState::M1(_) | RecommendedState::M2(_) | RecommendedState::M3(_) => {
                if default_ds.slave_only {
                    match self.port_state {
                        PortState::Listening => { /* do nothing */ }
                        PortState::Slave(_) | PortState::Passive => {
                            self.set_forced_port_state(PortState::Listening);

                            // consistent with Port<InBmca>::new()
                            let duration = self.config.announce_duration(&mut self.rng);
                            let reset_announce = PortAction::ResetAnnounceReceiptTimer { duration };
                            self.lifecycle.pending_action = actions![reset_announce];
                        }
                        PortState::Master(_) => {
                            let msg = "slave-only PTP port should not be in master state";
                            debug_assert!(!default_ds.slave_only, "{msg}");
                            log::error!("{msg}");
                        }
                    }
                } else {
                    match self.port_state {
                        PortState::Listening | PortState::Slave(_) | PortState::Passive => {
                            self.set_forced_port_state(PortState::Master(MasterState::new()));

                            // Immediately start sending announces and syncs
                            let duration = core::time::Duration::from_secs(0);
                            self.lifecycle.pending_action = actions![
                                PortAction::ResetAnnounceTimer { duration },
                                PortAction::ResetSyncTimer { duration }
                            ];
                        }
                        PortState::Master(_) => { /* do nothing */ }
                    }
                }
            }
            RecommendedState::P1(_) | RecommendedState::P2(_) => match self.port_state {
                PortState::Listening | PortState::Slave(_) | PortState::Master(_) => {
                    self.set_forced_port_state(PortState::Passive)
                }
                PortState::Passive => {}
            },
        }
    }
}

impl<'a, C, F: Filter, R: Rng> Port<InBmca<'a>, R, C, F> {
    /// Create a new port from a port dataset on a given interface.
    pub(crate) fn new(
        state_refcell: &'a AtomicRefCell<PtpInstanceState>,
        config: PortConfig,
        filter_config: F::Config,
        clock: C,
        port_identity: PortIdentity,
        mut rng: R,
    ) -> Self {
        let bmca = Bmca::new(config.announce_interval.as_duration().into(), port_identity);

        let duration = config.announce_duration(&mut rng);

        Port {
            config,
            filter_config,
            clock,
            port_identity,
            port_state: PortState::Listening,
            bmca,
            rng,
            packet_buffer: [0; MAX_DATA_LEN],
            lifecycle: InBmca {
                pending_action: actions![PortAction::ResetAnnounceReceiptTimer { duration }],
                local_best: None,
                state_refcell,
            },
        }
    }
}
