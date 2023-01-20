use crate::datastructures::common::ClockIdentity;
use crate::datastructures::common::ClockQuality;
use crate::datastructures::common::InstanceType;
use crate::time::Instant;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DefaultDS {
    pub(crate) clock_identity: ClockIdentity,
    number_ports: u16,
    pub(crate) clock_quality: ClockQuality,
    pub(crate) priority_1: u8,
    pub(crate) priority_2: u8,
    pub(crate) domain_number: u8,
    slave_only: bool,
    // TODO: u12
    pub(crate) sdo_id: u16,
    current_time: Instant,
    pub(crate) instance_enable: bool,
    external_port_configuration_enabled: bool,
    max_steps_removed: u8,
    pub(crate) instance_type: InstanceType,
}

impl DefaultDS {
    pub fn new_oc(
        clock_identity: ClockIdentity,
        priority_1: u8,
        priority_2: u8,
        domain_number: u8,
        slave_only: bool,
        sdo_id: u16,
    ) -> Self {
        DefaultDS {
            clock_identity,
            number_ports: 1,
            clock_quality: Default::default(),
            priority_1,
            priority_2,
            domain_number,
            slave_only,
            sdo_id,
            current_time: Default::default(),
            instance_enable: true,
            external_port_configuration_enabled: false,
            max_steps_removed: 255,
            instance_type: InstanceType::OrdinaryClock,
        }
    }

    pub fn new_bc(
        clock_identity: ClockIdentity,
        number_ports: u16,
        priority_1: u8,
        priority_2: u8,
        domain_number: u8,
        sdo_id: u16,
    ) -> Self {
        DefaultDS {
            clock_identity,
            number_ports,
            clock_quality: Default::default(),
            priority_1,
            priority_2,
            domain_number,
            // Not applicable
            slave_only: false,
            sdo_id,
            current_time: Default::default(),
            instance_enable: true,
            external_port_configuration_enabled: false,
            max_steps_removed: 255,
            instance_type: InstanceType::BoundaryClock,
        }
    }
}
