//! installed policy and the one adjacent revision allowed to finish in flight.

use crate::transport::signaling::Policy;
use crate::transport::wire::CodecError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallError {
    Invalid(CodecError),
    Rollback,
    Mutation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyUpdate {
    Unchanged,
    Installed {
        /// first policy and skipped revisions require all queued audio purged.
        purge_all: bool,
        removed_receivers: Vec<u16>,
        tx_changed: bool,
        self_monitor_changed: bool,
    },
}

#[derive(Clone)]
pub struct PolicyGate {
    local_player: u16,
    current: Option<Policy>,
    previous: Option<Policy>,
}

impl PolicyGate {
    pub fn new(local_player: u16) -> Self {
        Self {
            local_player,
            current: None,
            previous: None,
        }
    }

    pub fn current(&self) -> Option<&Policy> {
        self.current.as_ref()
    }

    pub fn identify(&mut self, local_player: u16) {
        self.local_player = local_player;
    }

    /// call only at a trusted roster/reset boundary, after clearing media state.
    pub fn reset(&mut self, local_player: u16) {
        *self = Self::new(local_player);
    }

    pub fn install(&mut self, policy: Policy) -> Result<PolicyUpdate, InstallError> {
        policy.validate().map_err(InstallError::Invalid)?;
        if let Some(current) = &self.current {
            if policy.audience.revision < current.audience.revision {
                return Err(InstallError::Rollback);
            }
            if policy.audience.revision == current.audience.revision {
                return if *current == policy {
                    Ok(PolicyUpdate::Unchanged)
                } else {
                    Err(InstallError::Mutation)
                };
            }
        }
        let adjacent = self.current.as_ref().is_some_and(|current| {
            current.audience.revision.checked_add(1) == Some(policy.audience.revision)
        });
        let mut removed_receivers = Vec::new();
        if let Some(current) = &self.current {
            removed_receivers.extend(current.audience.receive_from.iter().copied().filter(
                |sender| {
                    current.allows_receiver(*sender, self.local_player)
                        && !policy.allows_receiver(*sender, self.local_player)
                },
            ));
            if current.allows_receiver(self.local_player, self.local_player)
                && !policy.allows_receiver(self.local_player, self.local_player)
                && !removed_receivers.contains(&self.local_player)
            {
                removed_receivers.push(self.local_player);
                removed_receivers.sort_unstable();
            }
        }
        let update = PolicyUpdate::Installed {
            purge_all: !adjacent,
            removed_receivers,
            tx_changed: self
                .current
                .as_ref()
                .is_some_and(|current| current.tx_permitted != policy.tx_permitted),
            self_monitor_changed: self
                .current
                .as_ref()
                .is_some_and(|current| current.self_monitor != policy.self_monitor),
        };
        let old = self.current.replace(policy);
        self.previous = if adjacent { old } else { None };
        Ok(update)
    }

    /// recheck queued media too; installing a policy never relabels old audio.
    pub fn accepts(&self, sender: u16, revision: u64) -> bool {
        let Some(current) = &self.current else {
            return false;
        };
        if !current.allows_receiver(sender, self.local_player) {
            return false;
        }
        revision == current.audience.revision
            || self.previous.as_ref().is_some_and(|previous| {
                revision == previous.audience.revision
                    && previous.allows_receiver(sender, self.local_player)
            })
    }
}
