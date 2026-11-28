//! Who may ask this agent for a job (design 6.6, ADR-483): the acceptance check of a verified
//! instruction's operator against the registered owner, decided from the registration record's
//! preset. The control channel's gate applies it to a delivered command, and the resume of a held
//! job applies it to the recorded one (ADR-513), so both give the same answer.

use shared_crypto::registration::Preset;

/// What the agent does with a verified instruction's operator ID, decided once per session from
/// the record's preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperatorPolicy {
    /// The operator must be the registered owner.
    OwnerOnly,
    /// No agent-side check: the server's own permission test stands alone.
    Anyone,
}

impl OperatorPolicy {
    /// The policy of the record's preset. A name that is not a preset (the record's reader
    /// refuses one, so this is not reached) is owner-only: the stricter choice.
    pub(crate) fn of_preset(preset: &str) -> OperatorPolicy {
        match Preset::parse(preset) {
            Some(Preset::Personal | Preset::PersonalRemote) | None => OperatorPolicy::OwnerOnly,
            Some(Preset::Shared) => OperatorPolicy::Anyone,
        }
    }

    /// Whether an instruction signed for `operator_id` is accepted by an agent whose registered
    /// owner is `owner_id`: exact bytes, with no case folding or trimming.
    pub(crate) fn allows(self, operator_id: &str, owner_id: &str) -> bool {
        match self {
            OperatorPolicy::OwnerOnly => operator_id == owner_id,
            OperatorPolicy::Anyone => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_presets_accept_only_the_owner_byte_for_byte() {
        for preset in ["personal", "personalRemote"] {
            let policy = OperatorPolicy::of_preset(preset);
            assert!(policy.allows("owner-1", "owner-1"), "{preset}");
            assert!(!policy.allows("owner-2", "owner-1"), "{preset}");
            assert!(!policy.allows("Owner-1", "owner-1"), "{preset}");
            assert!(!policy.allows("owner-1 ", "owner-1"), "{preset}");
        }
    }

    #[test]
    fn shared_accepts_any_operator() {
        let policy = OperatorPolicy::of_preset("shared");
        assert!(policy.allows("someone-else", "owner-1"));
    }

    #[test]
    fn a_name_that_is_not_a_preset_is_owner_only() {
        let policy = OperatorPolicy::of_preset("nonsense");
        assert!(!policy.allows("someone-else", "owner-1"));
        assert!(policy.allows("owner-1", "owner-1"));
    }
}
