#[cfg(any(feature = "fuzzy", test))]
use arbitrary::{Arbitrary, Error as ArbitraryError, Unstructured};
use log::{error, warn};

use std::sync::{mpsc, Arc};

#[cfg(any(feature = "fuzzy", test))]
use std::result;

#[allow(unused_imports)]
use crate::broker::Shard;

use crate::broker::{InpSeqno, OutSeqno, QueueStatus, Session};

use crate::{v5, ClientID, PacketID};

/// Type implement the tx-handle for a message-queue.
#[derive(Clone)]
pub struct MsgTx {
    shard_id: u32,                 // message queue for shard
    tx: mpsc::SyncSender<Message>, // shard's incoming message queue
    waker: Arc<mio::Waker>,        // receiving shard's waker
    count: usize,
}

impl Drop for MsgTx {
    fn drop(&mut self) {
        if self.count > 0 {
            match self.waker.wake() {
                Ok(()) => (),
                Err(err) => {
                    error!("shard-{} waking the receiving shard: {}", self.shard_id, err)
                }
            }
        }
    }
}

impl MsgTx {
    pub fn try_sends(&mut self, msgs: Vec<Message>) -> QueueStatus<Message> {
        let mut iter = msgs.into_iter();
        loop {
            match iter.next() {
                Some(msg) => match self.tx.try_send(msg) {
                    Ok(()) => self.count += 1,
                    Err(mpsc::TrySendError::Full(msg)) => {
                        let mut msgs: Vec<Message> = Vec::from_iter(iter);
                        msgs.insert(0, msg);
                        break QueueStatus::Block(msgs);
                    }
                    Err(mpsc::TrySendError::Disconnected(msg)) => {
                        warn!("shard-{} shard disconnected ...", self.shard_id);
                        let mut msgs: Vec<Message> = Vec::from_iter(iter);
                        msgs.insert(0, msg);
                        break QueueStatus::Disconnected(msgs);
                    }
                },
                None => break QueueStatus::Ok(Vec::new()),
            }
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

/// Type implement the rx-handle for a message-queue.
pub struct MsgRx {
    shard_id: u32, // message queue for shard.
    msg_batch_size: usize,
    rx: mpsc::Receiver<Message>,
}

impl MsgRx {
    pub fn try_recvs(&self) -> QueueStatus<Message> {
        let mut msgs = Vec::new(); // TODO: with_capacity ?
        loop {
            match self.rx.try_recv() {
                Ok(msg) if msgs.len() < self.msg_batch_size => msgs.push(msg),
                Ok(msg) => {
                    msgs.push(msg);
                    break QueueStatus::Ok(msgs);
                }
                Err(mpsc::TryRecvError::Empty) => break QueueStatus::Block(msgs),
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("shard-{} shard disconnected ...", self.shard_id);
                    break QueueStatus::Disconnected(msgs);
                }
            }
        }
    }
}

/// Message is a unit of communication between shards hosted on the same node.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Message {
    /// Packets that are generated by sessions locally and sent to clients, doesn't cross
    /// session boundary.
    ///
    /// CONNACK, PUBLISH-ack, SUBACK, UNSUBACK, PINGRESP, AUTH packets.
    ClientAck { packet: v5::Packet },
    /// PUBLISH Packets received from clients and routed to other local sessions.
    Routed {
        src_client_id: ClientID,     // sending client-id
        src_shard_id: u32,           // sending shard
        inp_seqno: InpSeqno,         // shard's inp_seqno
        packet_id: Option<PacketID>, // from publishing client, refer inp_qos1, inp_qos2
        publish: v5::Publish,        // publish packet, as received from publishing client
        subscriptions: Vec<v5::Subscription>,
    },
    /// Message that is periodically published by a session to other local shards.
    LocalAck {
        shard_id: u32,        // shard sending the acknowledgement
        last_acked: InpSeqno, // from publishing-shard.
    },
    /// PUBLISH Packets received from clients and routed to other local sessions.
    Packet {
        out_seqno: OutSeqno,
        publish: v5::Publish,
    },
}

#[cfg(any(feature = "fuzzy", test))]
impl<'a> Arbitrary<'a> for Message {
    fn arbitrary(uns: &mut Unstructured<'a>) -> result::Result<Self, ArbitraryError> {
        let val = match uns.arbitrary::<u8>()? % 3 {
            0 => Message::LocalAck {
                shard_id: uns.arbitrary()?,
                last_acked: uns.arbitrary()?,
            },
            1 => Message::Packet {
                client_id: uns.arbitrary()?,
                shard_id: uns.arbitrary()?,
                seqno: uns.arbitrary()?,
                packet_id: uns.arbitrary()?,
                subscriptions: uns.arbitrary()?,
                packet: v5::Packet::Publish(uns.arbitrary()?),
            },
            2 => Message::ClientAck {
                packet: match uns.arbitrary::<u8>()? % 9 {
                    0 => v5::Packet::ConnAck(uns.arbitrary()?),
                    1 => v5::Packet::PubAck(uns.arbitrary()?),
                    2 => v5::Packet::PubRec(uns.arbitrary()?),
                    3 => v5::Packet::PubRel(uns.arbitrary()?),
                    4 => v5::Packet::PubComp(uns.arbitrary()?),
                    5 => v5::Packet::SubAck(uns.arbitrary()?),
                    6 => v5::Packet::UnsubAck(uns.arbitrary()?),
                    7 => v5::Packet::PingResp,
                    8 => v5::Packet::Auth(uns.arbitrary()?),
                    _ => unreachable!(),
                },
            },
            _ => unreachable!(),
        };

        Ok(val)
    }
}

impl Message {
    /// Create a new Message::ClientAck value.
    pub fn new_client_ack(packet: v5::Packet) -> Message {
        Message::ClientAck { packet }
    }

    /// Create a new Message::Routed value.
    pub fn new_routed(
        sess: &Session,
        inp_seqno: InpSeqno,
        subscriptions: Vec<v5::Subscription>,
        publish: v5::Publish,
    ) -> Message {
        Message::Routed {
            src_client_id: sess.as_client_id().clone(),
            src_shard_id: sess.to_shard_id(),
            inp_seqno,
            packet_id: publish.packet_id,
            publish,
            subscriptions,
        }
    }

    /// Create a new Message::ClientAck value.
    pub fn new_packet(out_seqno: InpSeqno, publish: v5::Publish) -> Message {
        Message::Packet { out_seqno, publish }
    }

    /// Return the packet within this message. Only applicable in ClientAck and Packet
    /// variants, shall panic if otherwise.
    pub fn into_packet(self) -> v5::Packet {
        match self {
            Message::ClientAck { packet } => packet,
            Message::Packet { publish, .. } => v5::Packet::Publish(publish),
            _ => unreachable!(),
        }
    }
}

/// Create a message-queue for shard `shard_id` that can hold upto `size` messages.
///
/// `waker` is attached to the [Shard] thread receiving this messages from the queue.
/// When MsgTx is dropped, thread will be woken up using `waker`.
pub fn msg_channel(shard_id: u32, size: usize, waker: Arc<mio::Waker>) -> (MsgTx, MsgRx) {
    let (tx, rx) = mpsc::sync_channel(size);
    let msg_tx = MsgTx { shard_id, tx, waker, count: usize::default() };
    let msg_rx = MsgRx { shard_id, msg_batch_size: size, rx };

    (msg_tx, msg_rx)
}
