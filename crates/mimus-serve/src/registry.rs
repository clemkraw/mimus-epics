//! The per-connection introspection registry.
//!
//! PVA caches type descriptors per connection. The first monitor for a
//! given type carries the full descriptor; subsequent monitors carry only
//! its cached identifier. For 100,000 channels of the same type on one
//! connection, that means one full descriptor and 99,999 identifiers.
//!
//! This is a key session rule for MIMUS to reproduce accurately. Getting
//! it wrong distorts subscription setup costs, the phase that dominates
//! startup for clients connecting to a million channels. The resulting
//! estimate can be wrong by orders of magnitude in either direction.

use mimus_profile::DescriptorPolicy;

/// Tracks which templates have sent their full descriptor on this connection.
#[derive(Debug)]
pub struct Registry {
    policy: DescriptorPolicy,
    /// Indexed by template; set to `true` after its full descriptor is sent.
    introduced: Vec<bool>,
    full_sent: u64,
    id_only_sent: u64,
}

impl Registry {
    pub fn new(templates: usize, policy: DescriptorPolicy) -> Self {
        Self {
            policy,
            introduced: vec![false; templates],
            full_sent: 0,
            id_only_sent: 0,
        }
    }

    /// Returns the descriptor bytes to send in a monitor init for `template`.
    ///
    /// Returns `None` if the policy requires the identifier-only form but
    /// the profile does not contain one. This means the study phase never
    /// observed a second channel of this type. Refusing is intentional:
    /// falling back to the full descriptor would misrepresent the connection's
    /// setup cost without giving downstream code any indication of the error.
    pub fn descriptor<'a>(
        &mut self,
        template: u16,
        full: &'a [u8],
        id_only: Option<&'a [u8]>,
    ) -> Option<&'a [u8]> {
        let idx = template as usize;
        let first = !self.introduced.get(idx).copied().unwrap_or(true);

        if first || self.policy == DescriptorPolicy::AlwaysFull {
            if idx < self.introduced.len() {
                self.introduced[idx] = true;
            }
            self.full_sent += 1;
            return Some(full);
        }

        let bytes = id_only?;
        self.id_only_sent += 1;
        Some(bytes)
    }

    /// Clears the descriptor cache state.
    ///
    /// A reconnect starts with an empty cache on both sides. Resetting
    /// ensures the server sends full descriptors again instead of
    /// identifiers the newly connected client has not received.
    pub fn reset(&mut self) {
        self.introduced.iter_mut().for_each(|v| *v = false);
    }

    #[inline]
    pub fn full_sent(&self) -> u64 {
        self.full_sent
    }

    #[inline]
    pub fn id_only_sent(&self) -> u64 {
        self.id_only_sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &[u8] = &[0xFD, 0x01, 0x00];
    const ID: &[u8] = &[0xFE, 0x01, 0x00];

    #[test]
    fn the_first_channel_of_a_type_gets_the_full_descriptor() {
        let mut r = Registry::new(2, DescriptorPolicy::FullThenId);
        assert_eq!(r.descriptor(0, FULL, Some(ID)), Some(FULL));
    }

    #[test]
    fn later_channels_of_the_same_type_get_the_identifier() {
        let mut r = Registry::new(2, DescriptorPolicy::FullThenId);
        r.descriptor(0, FULL, Some(ID));
        for _ in 0..1000 {
            assert_eq!(r.descriptor(0, FULL, Some(ID)), Some(ID));
        }
        assert_eq!(r.full_sent(), 1);
        assert_eq!(r.id_only_sent(), 1000);
    }

    #[test]
    fn each_type_introduces_itself_once() {
        let mut r = Registry::new(3, DescriptorPolicy::FullThenId);
        assert_eq!(r.descriptor(0, FULL, Some(ID)), Some(FULL));
        assert_eq!(r.descriptor(1, FULL, Some(ID)), Some(FULL));
        assert_eq!(r.descriptor(0, FULL, Some(ID)), Some(ID));
        assert_eq!(r.descriptor(1, FULL, Some(ID)), Some(ID));
        assert_eq!(r.full_sent(), 2);
    }

    #[test]
    fn a_reconnect_starts_from_an_empty_cache() {
        let mut r = Registry::new(1, DescriptorPolicy::FullThenId);
        r.descriptor(0, FULL, Some(ID));
        r.reset();
        assert_eq!(
            r.descriptor(0, FULL, Some(ID)),
            Some(FULL),
            "the client's cache is empty too"
        );
    }

    #[test]
    fn a_missing_identifier_variant_is_refused_not_papered_over() {
        let mut r = Registry::new(1, DescriptorPolicy::FullThenId);
        assert_eq!(r.descriptor(0, FULL, None), Some(FULL));
        assert_eq!(
            r.descriptor(0, FULL, None),
            None,
            "no id-only variant means the study never saw a second channel"
        );
    }

    #[test]
    fn always_full_never_caches() {
        let mut r = Registry::new(1, DescriptorPolicy::AlwaysFull);
        for _ in 0..10 {
            assert_eq!(r.descriptor(0, FULL, Some(ID)), Some(FULL));
        }
        assert_eq!(r.id_only_sent(), 0);
    }

    #[test]
    fn an_out_of_range_template_does_not_panic() {
        let mut r = Registry::new(1, DescriptorPolicy::FullThenId);
        assert_eq!(r.descriptor(99, FULL, Some(ID)), Some(ID));
    }
}
