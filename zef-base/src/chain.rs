// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    committee::Committee,
    crypto::*,
    ensure,
    error::Error,
    execution::{Balance, ChainAdminStatus, Effect, ExecutionState, ADMIN_CHANNEL},
    manager::ChainManager,
    messages::*,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// The state of a chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(Eq, PartialEq))]
pub struct ChainState {
    /// How the chain was created. May be unknown for inactive chains.
    pub description: Option<ChainDescription>,
    /// Execution state.
    pub state: ExecutionState,
    /// Hash of the execution state.
    pub state_hash: HashValue,
    /// Hash of the latest certified block in this chain, if any.
    pub block_hash: Option<HashValue>,
    /// Sequence number tracking blocks.
    pub next_block_height: BlockHeight,

    /// Hashes of all certified blocks for this sender.
    /// This ends with `block_hash` and has length `usize::from(next_block_height)`.
    pub confirmed_log: Vec<HashValue>,
    /// Hashes of all certified blocks known as a receiver (local ordering).
    pub received_log: Vec<HashValue>,

    /// Mailboxes used to send messages, indexed by recipient.
    pub outboxes: HashMap<ChainId, OutboxState>,
    /// Mailboxes used to receive messages indexed by their origin.
    pub inboxes: HashMap<Origin, InboxState>,
    /// Channels able to multicast messages to subscribers.
    pub channels: HashMap<String, ChannelState>,
}

/// An outbox used to send messages to another chain. NOTE: Messages are implied by the
/// execution of blocks, so currently we just send the certified blocks over and let the
/// receivers figure out what was the message for them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(Eq, PartialEq))]
pub struct OutboxState {
    /// Keep sending these certified blocks of ours until they are acknowledged by
    /// receivers.
    pub queue: VecDeque<BlockHeight>,
}

/// An inbox used to receive and execute messages from another chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(Eq, PartialEq))]
pub struct InboxState {
    /// We have already received the cross-chain requests and enqueued all the messages
    /// below this height.
    pub next_height_to_receive: BlockHeight,
    /// These events have been received but not yet picked by a block to be executed.
    pub received_events: VecDeque<Event>,
    /// These events have been executed but the cross-chain requests have not been
    /// received yet.
    pub expected_events: VecDeque<Event>,
}

/// The state of a channel followed by subscribers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(Eq, PartialEq))]
pub struct ChannelState {
    /// The subscribers and whether they have received the latest update yet.
    pub subscribers: HashMap<ChainId, bool>,
    /// The latest block height to communicate, if any.
    pub block_height: Option<BlockHeight>,
}

/// A message sent by some (unspecified) chain at a particular height and index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "test"), derive(Eq, PartialEq))]
pub struct Event {
    /// The height of the block that created the event.
    pub height: BlockHeight,
    /// The index of the effect.
    pub index: usize,
    /// The effect of the event.
    pub effect: Effect,
}

impl ChainState {
    pub fn new(id: ChainId) -> Self {
        let state = ExecutionState::new(id);
        let state_hash = HashValue::new(&state);
        Self {
            description: None,
            state,
            state_hash,
            block_hash: None,
            next_block_height: BlockHeight::default(),
            confirmed_log: Vec::new(),
            received_log: Vec::new(),
            inboxes: HashMap::new(),
            outboxes: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    pub fn create(
        committee: Committee,
        admin_id: ChainId,
        description: ChainDescription,
        owner: Owner,
        balance: Balance,
    ) -> Self {
        let mut chain = Self::new(description.into());
        chain.description = Some(description);
        chain.state.epoch = Some(Epoch::from(0));
        if ChainId::from(description) == admin_id {
            chain.state.admin_status = Some(ChainAdminStatus::Managing);
            chain
                .channels
                .insert(ADMIN_CHANNEL.into(), ChannelState::default());
        } else {
            chain.state.admin_status = Some(ChainAdminStatus::ManagedBy { admin_id });
        }
        chain.state.committees.insert(Epoch::from(0), committee);
        chain.state.manager = ChainManager::single(owner);
        chain.state.balance = balance;
        chain.state_hash = HashValue::new(&chain.state);
        assert!(chain.is_active());
        chain
    }

    /// Invariant for the states of active chains.
    pub fn is_active(&self) -> bool {
        self.description.is_some()
            && self.state.manager.is_active()
            && self.state.epoch.is_some()
            && self
                .state
                .committees
                .contains_key(self.state.epoch.as_ref().unwrap())
            && self.state.admin_status.is_some()
    }

    pub fn make_chain_info(&self, key_pair: Option<&KeyPair>) -> ChainInfoResponse {
        let info = ChainInfo {
            chain_id: self.state.chain_id,
            epoch: self.state.epoch,
            description: self.description,
            manager: self.state.manager.clone(),
            balance: self.state.balance,
            block_hash: self.block_hash,
            next_block_height: self.next_block_height,
            state_hash: self.state_hash,
            requested_execution_state: None,
            requested_pending_messages: Vec::new(),
            requested_sent_certificates: Vec::new(),
            count_received_certificates: self.received_log.len(),
            requested_received_certificates: Vec::new(),
        };
        ChainInfoResponse::new(info, key_pair)
    }

    /// Verify that this chain is up-to-date and all the messages executed ahead of time
    /// have been properly received by now.
    pub fn validate_incoming_messages(&self) -> Result<(), Error> {
        for (origin, inbox) in &self.inboxes {
            ensure!(
                inbox.expected_events.is_empty(),
                Error::MissingCrossChainUpdate {
                    origin: origin.clone(),
                    height: inbox.expected_events.front().unwrap().height,
                }
            );
        }
        Ok(())
    }

    /// Whether an invalid operation for this block can become valid later.
    pub fn is_retriable_validation_error(error: &Error) -> bool {
        matches!(error, Error::MissingCrossChainUpdate { .. })
    }

    /// Schedule operations to be executed as a recipient, unless this block was already
    /// processed. Returns true if the call changed the chain state. Operations must be
    /// received by order of heights and indices.
    pub fn receive_block(
        &mut self,
        origin: &Origin,
        height: BlockHeight,
        effects: Vec<Effect>,
        key: HashValue,
    ) -> Result<bool, Error> {
        if let Origin::Channel(channel) = origin {
            if !self.state.subscriptions.contains_key(channel) {
                // Refuse messages from channels that we are currently not subscribed to.
                // This is important for other validators to be synchronizable.
                log::warn!(
                    "Ignoring message to {} from unsubscribed channel {:?} at height {}",
                    self.state.chain_id,
                    channel,
                    height
                );
                return Ok(false);
            }
        }
        let inbox = self.inboxes.entry(origin.clone()).or_default();
        if height < inbox.next_height_to_receive {
            // We have already received this block.
            log::warn!(
                "Ignoring past messages to {:?} from {:?} at height {}",
                self.state.chain_id,
                origin,
                height
            );
            return Ok(false);
        }
        log::trace!(
            "Processing new messages to {:?} from {:?} at height {}",
            self.state.chain_id,
            origin,
            height
        );
        // Mark the block as received.
        inbox.next_height_to_receive = height.try_add_one()?;
        self.received_log.push(key);

        for (index, effect) in effects.into_iter().enumerate() {
            // Skip events that have provably no effect on this recipient.
            if !self.state.is_recipient(&effect) {
                continue;
            }
            // Chain creation and channel subscriptions are special effects that are executed (only) in this callback.
            // For simplicity, they will still appear as incoming messages in new blocks, as the other effects.
            match &effect {
                Effect::OpenChain {
                    id,
                    owner,
                    epoch,
                    committees,
                    admin_id,
                } if id == &self.state.chain_id => {
                    // Guaranteed under BFT assumptions.
                    assert!(self.description.is_none());
                    assert!(!self.state.manager.is_active());
                    assert!(self.state.committees.is_empty());
                    let description = ChainDescription::Child(EffectId {
                        chain_id: origin.sender(),
                        height,
                        index,
                    });
                    assert_eq!(self.state.chain_id, description.into());
                    self.description = Some(description);
                    self.state.epoch = Some(*epoch);
                    self.state.committees = committees.clone();
                    self.state.admin_status = Some(ChainAdminStatus::ManagedBy {
                        admin_id: *admin_id,
                    });
                    self.state.subscriptions.insert(
                        ChannelId {
                            chain_id: *admin_id,
                            name: ADMIN_CHANNEL.into(),
                        },
                        (),
                    );
                    self.state.manager = ChainManager::single(*owner);
                    self.state_hash = HashValue::new(&self.state);
                }
                Effect::Subscribe { id, channel } if channel.chain_id == self.state.chain_id => {
                    let channel = self
                        .channels
                        .entry(channel.name.clone())
                        .or_insert_with(ChannelState::default);
                    // Request past and future messages from this channel.
                    channel.subscribers.insert(*id, false);
                }
                Effect::Unsubscribe { id, channel } if channel.chain_id == self.state.chain_id => {
                    let channel = self
                        .channels
                        .entry(channel.name.clone())
                        .or_insert_with(ChannelState::default);
                    // Remove subscriber.
                    channel.subscribers.remove(id);
                }
                _ => (),
            }
            // Find if the message was executed ahead of time.
            match inbox.expected_events.front() {
                Some(event) => {
                    if height == event.height && index == event.index {
                        // We already executed this message by anticipation. Remove it from the queue.
                        assert_eq!(effect, event.effect, "Unexpected effect in certified block");
                        inbox.expected_events.pop_front();
                    } else {
                        // The receiver has already executed a later event from the same
                        // sender ahead of time so we should skip this one.
                        assert!(
                            (height, index) < (event.height, event.index),
                            "Unexpected event order in certified block"
                        );
                    }
                }
                None => {
                    // Otherwise, schedule the message for execution.
                    inbox.received_events.push_back(Event {
                        height,
                        index,
                        effect,
                    });
                }
            }
        }
        Ok(true)
    }

    /// Verify that the incoming_messages are in the right order. This matters for inbox
    /// invariants, notably the fact that inbox.expected_events is sorted.
    fn check_incoming_messages(&self, messages: &[MessageGroup]) -> Result<(), Error> {
        let mut next_messages: HashMap<Origin, (BlockHeight, usize)> = HashMap::new();
        for message_group in messages {
            let next_message = next_messages
                .entry(message_group.origin.clone())
                .or_default();
            for (message_index, _) in &message_group.effects {
                ensure!(
                    (message_group.height, *message_index) >= *next_message,
                    Error::InvalidMessageOrder {
                        origin: message_group.origin.clone(),
                        height: message_group.height,
                        index: *message_index,
                    }
                );
                *next_message = (message_group.height, *message_index + 1);
            }
        }
        Ok(())
    }

    /// Execute a new block: first the incoming messages, then the main operation.
    /// * Modifies the state of inboxes, outboxes, and channels, if needed.
    /// * As usual, in case of errors, `self` may not be consistent any more and should be thrown away.
    /// * Returns the list of effects caused by the block being executed.
    pub fn execute_block(&mut self, block: &Block) -> Result<Vec<Effect>, Error> {
        let mut effects = Vec::new();
        // First, process incoming messages.
        self.check_incoming_messages(&block.incoming_messages)?;

        for message_group in &block.incoming_messages {
            let inbox = self
                .inboxes
                .entry(message_group.origin.clone())
                .or_default();

            for (message_index, message_effect) in &message_group.effects {
                // Receivers are allowed to skip events from the received queue.
                while let Some(Event {
                    height,
                    index,
                    effect: _,
                }) = inbox.received_events.front()
                {
                    if *height > message_group.height
                        || (*height == message_group.height && index >= message_index)
                    {
                        break;
                    }
                    assert!((*height, index) < (message_group.height, message_index));
                    let event = inbox.received_events.pop_front().unwrap();
                    log::trace!("Skipping received event: {:?}", event);
                }
                // Reconcile the event with the received queue, or mark it as "expected".
                match inbox.received_events.front() {
                    Some(Event {
                        height,
                        index,
                        effect,
                    }) => {
                        ensure!(
                            message_group.height == *height && message_index == index,
                            Error::InvalidMessage {
                                origin: message_group.origin.clone(),
                                height: message_group.height,
                                index: *message_index,
                                expected_height: *height,
                                expected_index: *index,
                            }
                        );
                        ensure!(
                            message_effect == effect,
                            Error::InvalidMessageContent {
                                origin: message_group.origin.clone(),
                                height: message_group.height,
                                index: *message_index,
                            }
                        );
                        inbox.received_events.pop_front().unwrap();
                    }
                    None => {
                        inbox.expected_events.push_back(Event {
                            height: message_group.height,
                            index: *message_index,
                            effect: message_effect.clone(),
                        });
                    }
                }
                // Execute the received effect.
                self.state
                    .apply_effect(self.state.chain_id, message_effect)?;
            }
        }
        // Second, execute the operations in the block and remember the recipients to notify.
        for (index, operation) in block.operations.iter().enumerate() {
            let application =
                self.state
                    .apply_operation(block.chain_id, block.height, index, operation)?;
            // When we unsubscribe from a channel, the corresponding inbox must be flushed
            // immediately so that we don't accept incoming messages until we subscribe again.
            for effect in &application.effects {
                if let Effect::Unsubscribe { channel, .. } = effect {
                    let origin = Origin::Channel(channel.clone());
                    self.inboxes.remove(&origin);
                }
            }
            // Record the effects of the execution.
            effects.extend(application.effects);
            // Update the outboxes.
            for recipient in application.recipients {
                let queue = &mut self.outboxes.entry(recipient).or_default().queue;
                // Schedule a message at this height if we haven't already.
                if queue.back() != Some(&block.height) {
                    queue.push_back(block.height);
                }
            }
            // Update the channels.
            for name in application.need_channel_broadcast {
                let channel = self
                    .channels
                    .entry(name)
                    .or_insert_with(ChannelState::default);
                channel.subscribers.values_mut().for_each(|v| *v = false);
                channel.block_height = Some(block.height);
            }
        }
        // Last, recompute the state hash.
        self.state_hash = HashValue::new(&self.state);
        Ok(effects)
    }
}
