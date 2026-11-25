//! Protocol command codes.
//!
//! Only commands handled by MIMUS are named here. Other PVAccess commands
//! exist but are intentionally omitted to avoid implying unsupported
//! functionality. This list defines the scope of MIMUS's protocol support;
//! `PUT` and `RPC` are named only so that MIMUS can refuse them.

/// Application commands.
pub const CONNECTION_VALIDATION: u8 = 0x01;
pub const ECHO: u8 = 0x02;
pub const SEARCH: u8 = 0x03;
pub const SEARCH_RESPONSE: u8 = 0x04;
pub const CREATE_CHANNEL: u8 = 0x07;
pub const DESTROY_CHANNEL: u8 = 0x08;
pub const CONNECTION_VALIDATED: u8 = 0x09;
pub const GET: u8 = 0x0A;
pub const PUT: u8 = 0x0B;
pub const MONITOR: u8 = 0x0D;
pub const DESTROY_REQUEST: u8 = 0x0F;
pub const GET_FIELD: u8 = 0x11;
pub const MESSAGE: u8 = 0x12;
pub const RPC: u8 = 0x14;

/// Control commands. Their data is stored in the header; no payload follows.
pub const CTRL_MARK_TOTAL_SENT: u8 = 0x00;
pub const CTRL_ACK_RECEIVED: u8 = 0x01;
pub const CTRL_SET_BYTE_ORDER: u8 = 0x02;
pub const CTRL_ECHO_REQUEST: u8 = 0x03;
pub const CTRL_ECHO_RESPONSE: u8 = 0x04;

/// Get sub-command bits.
///
/// A read consists of two messages: an INIT message describing the type,
/// followed by a message carrying the value. Without INIT, the message
/// requests the value. This applies to zero, `0x40`, and `DESTROY`,
/// which also marks the client's final request for the operation.
pub mod get {
    pub const INIT: u8 = 0x08;
    pub const DESTROY: u8 = 0x10;
}

/// Put sub-command bits: those of [`get`], and one of its own. A PUT and
/// an RPC share the INIT and DESTROY of a GET.
pub mod put {
    /// Without INIT: asks for the value about to be written over, rather
    /// than writing one. A PVXS client asks before it builds its value from
    /// the one present.
    pub const GET: u8 = 0x40;
}

/// Monitor sub-command bits. The first byte after the request ID.
///
/// START and STOP share a bit, as specified by PVAccess:
///
/// > `subcommand & 0x44 == 0x44`: Stopped → Running.
/// > `subcommand & 0x44 == 0x04`: Running → Stopped.
///
/// Therefore, `0x04` alone means STOP, not START. Using it to start
/// a subscription creates the monitor but immediately stops it, so
/// no updates are ever received. These values must be treated as
/// state encodings, not independent flags.
pub mod monitor {
    /// Initialize the subscription and describe its type.
    pub const INIT: u8 = 0x08;
    /// Transition from Stopped to Running. Both bits are required.
    pub const START: u8 = 0x44;
    /// Transition from Running to Stopped.
    pub const STOP: u8 = 0x04;
    /// Mask used to identify the subscription state.
    pub const STATE_MASK: u8 = 0x44;
    /// Terminate the subscription.
    pub const DESTROY: u8 = 0x10;
    /// On INIT, a pipelined subscription whose queue size follows the
    /// `pvRequest`; on any other message, a credit that follows the
    /// sub-command.
    pub const PIPELINE: u8 = 0x80;
}

/// Human-readable name for a command, for logs and diagnostics.
pub fn name(command: u8, is_control: bool) -> &'static str {
    if is_control {
        return match command {
            CTRL_MARK_TOTAL_SENT => "CTRL_MARK_TOTAL_SENT",
            CTRL_ACK_RECEIVED => "CTRL_ACK_RECEIVED",
            CTRL_SET_BYTE_ORDER => "CTRL_SET_BYTE_ORDER",
            CTRL_ECHO_REQUEST => "CTRL_ECHO_REQUEST",
            CTRL_ECHO_RESPONSE => "CTRL_ECHO_RESPONSE",
            _ => "CTRL_UNKNOWN",
        };
    }
    match command {
        CONNECTION_VALIDATION => "CONNECTION_VALIDATION",
        ECHO => "ECHO",
        SEARCH => "SEARCH",
        SEARCH_RESPONSE => "SEARCH_RESPONSE",
        CREATE_CHANNEL => "CREATE_CHANNEL",
        DESTROY_CHANNEL => "DESTROY_CHANNEL",
        CONNECTION_VALIDATED => "CONNECTION_VALIDATED",
        GET => "GET",
        MONITOR => "MONITOR",
        DESTROY_REQUEST => "DESTROY_REQUEST",
        GET_FIELD => "GET_FIELD",
        MESSAGE => "MESSAGE",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_application_spaces_are_separate() {
        // 0x01 identifies different commands depending on the control flag.
        // Dispatching on the command byte alone is incorrect.
        assert_eq!(name(0x01, false), "CONNECTION_VALIDATION");
        assert_eq!(name(0x01, true), "CTRL_ACK_RECEIVED");
    }

    #[test]
    fn unknown_commands_are_named_not_panicked() {
        assert_eq!(name(0xEE, false), "UNKNOWN");
        assert_eq!(name(0xEE, true), "CTRL_UNKNOWN");
    }

    #[test]
    fn start_and_stop_are_told_apart_by_a_shared_mask() {
        // 0x04 means STOP, not START. Using it to start a subscription
        // creates and immediately stops it without triggering an error.
        assert_eq!(monitor::START & monitor::STATE_MASK, monitor::STATE_MASK);
        assert_eq!(monitor::STOP & monitor::STATE_MASK, monitor::STOP);
        assert_ne!(monitor::START, monitor::STOP);
    }

    #[test]
    fn a_pipelined_start_sets_the_flow_control_bit_as_well() {
        let pipelined = monitor::START | monitor::PIPELINE;
        assert_eq!(pipelined, 0xC4);
        assert_eq!(pipelined & monitor::STATE_MASK, monitor::STATE_MASK);
        assert_ne!(pipelined & monitor::PIPELINE, 0);
    }
}
