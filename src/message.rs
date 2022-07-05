use std::collections::{BTreeMap, VecDeque};
use std::{sync::mpsc, time};

use crate::{v5, ClientID, PacketID};

pub type MsgTx = mpsc::SyncSender<Message>;
pub type MsgRx = mpsc::Receiver<Message>;

// Note that Session::timestamp is related to ClientInp::timestamp.
pub struct ClientInp {
    // Monotonically increasing `seqno`, starting from 1, that is bumped up for every
    // incoming message. This seqno shall be attached to every Message::Packet.
    pub seqno: u64,
    // This index is a collection of un-acked collection of incoming packets.
    // All incoming SUBSCRIBE, UNSUBSCRIBE, PUBLISH (QoS-!,2) shall be indexed here
    // using the packet_id. It will be deleted only when corresponding ACK is queued
    // in the outbound channel. And this ACK shall be dispatched Only when:
    // * PUBLISH-ack is received from other local-sessions.
    // * SUBSCRIBE/UNSUBSCRIBE committed to SessionState.
    //
    // Periodically purge this index based on `min(timestamp:seqno)`. To effeciently
    // implement this index-purge cycle, we use the `timestamp` collection. When ever
    // PUBLISH packet is sent to other local-sessions, `timestamp` index will be updated
    // for ClientID with (0, Instant::now()), provided it does not already have an entry
    // for ClientID.
    //
    // This index is also used to detect duplicate PUBLISH, SUBSCRIBE, and UNSUBSCRIBE
    // packets.
    pub index: BTreeMap<PacketID, Message>,
    // For N active sessions in this node, there snall be N-1 entries in this index.
    //
    // Entry-value is (ClientInp::seqno, last-ack-instant), where seqno cycles-back from
    // the other local-session via Messages::LocalAck.
    //
    // Periodically, the minimum value of this list shall be computed and Messages older
    // than the computed-minium shall be purged from the `index`.
    //
    // Entries whose `seqno` is ZERO and `lask-ack-instant` is older that configured
    // limit shall be considered dead session and cluster shall be consulted for
    // cleanup.
    pub timestamp: BTreeMap<ClientID, (u64, time::Instant)>,
}

pub struct ClientOut {
    // Monotonically increasing `seqno`, starting from 1, that is bumped up for every
    // out going message for this session. This will also be sent in PUBLISH UserProp.
    //
    // TODO: can we fold this into consensus seqno ?
    pub seqno: u64,
    // This index is essentially un-acked collection of inflight PUBLISH messages.
    //
    // All incoming messages will be indexed here using monotonically increasing
    // sequence number tracked by `ClientOut::seqno`.
    //
    // Note that before indexing message, its `seqno` shall be overwritten from
    // ClientOut::seqno, and its `packet_id` field will be overwritten with the one
    // procured from `next_packet_id` cache.
    //
    // Note that length of this collection is only as high as the allowed limit of
    // concurrent PUBLISH.
    pub index: BTreeMap<PacketID, Message>,
    // Rolling 16-bit packet-identifier, packet-id ZERO is not used and reserved.
    //
    // This value is incremented for every new PUBLISH(qos>0), SUBSCRIBE, UNSUBSCRIBE
    // messages that is going out to the client.
    //
    // We don't increment this value if index.len() exceeds the `receive_maximum`
    // set by the client.
    pub next_packet_id: PacketID,
    // Back log of messages that needs to be flushed out to the client. All messages
    // meant for client first lands here.
    //
    // CONNACK, PUBLISH, PUBLISH-ack, SUBACK, UNSUBACK, PINGRESP, DISCONNECT, AUTH
    pub back_log: VecDeque<Message>,
}

pub enum Message {
    /// Message that is periodically, say every 30ms, published by a session to other
    /// local sessions.
    LocalAck {
        client_id: ClientID,
        seqno: u64, // sending-session -> receive-session -> sending-session
        instant: time::Instant, // instant the ack is sent from local session.
    },
    /// Packets that are received from clients and sent to other local sessions.
    /// Packets that are received from other local session and meant for this client.
    /// Only PUBLISH packets.
    Packet {
        client_id: ClientID,
        shard_id: u32,
        seqno: u64,          // from ClientInp::seqno or ClientOut::seqno,
        packet_id: PacketID, // from ClientInp or ClientOut
        packet: v5::Packet,
    },
    /// Packets that are generated by sessions locally and sent to clients.
    ///
    /// CONNACK, PUBLISH-ack, SUBACK, UNSUBACK, PINGRESP, DISCONNECT, AUTH packets.
    ClientAck { packet: v5::Packet },
}

impl Message {
    pub fn new_client_ack(packet: v5::Packet) -> Message {
        Message::ClientAck { packet }
    }

    pub fn set_seqno(&mut self, new_seqno: u64, new_packet_id: PacketID) {
        match self {
            Message::Packet { seqno, packet_id, .. } => {
                *seqno = new_seqno;
                *packet_id = new_packet_id;
            }
            _ => unreachable!(),
        }
    }
}

#[inline]
pub fn msg_channel(size: usize) -> (MsgTx, MsgRx) {
    mpsc::sync_channel(size)
}