use log::{debug, info, trace};
use mio::event::Events;
use uuid::Uuid;

use std::sync::{mpsc, Arc};
use std::{collections::BTreeMap, net, path, time};

use crate::thread::{Rx, Thread, Threadable, Tx};
use crate::{rebalance, util, v5};
use crate::{AppTx, Config, ConfigNode, Hostable, RetainedTrie, SubscribedTrie};
use crate::{Flusher, Listener, Shard, Ticker};

use crate::{Error, ErrorKind, Result};

// TODO: Review .ok() .unwrap() allow_panic!(), panic!() and unreachable!() calls.
// TODO: Review assert macro calls.
// TODO: Review `as` type-casting for numbers.
// TODO: Validate and document all thread handles, cluster, listener, flusher, shard,
//       miot.
// TODO: Handle retain-messages in Will, Publish, Subscribe scenarios, retain_available.

type ThreadRx = Rx<Request, Result<Response>>;

/// Cluster is the global configuration state for multi-node MQTT cluster.
pub struct Cluster {
    /// Refer [Config::name]
    pub name: String,
    prefix: String,
    config: Config,
    inner: Inner,
}

enum Inner {
    Init,
    // Help by application.
    Handle(Arc<mio::Waker>, Thread<Cluster, Request, Result<Response>>),
    // Held by Listener, Handshake and Shard.
    Tx(Arc<mio::Waker>, Tx<Request, Result<Response>>),
    // Thread
    Main(RunLoop),
    // Held by Application, replacing both Handle and Main.
    Close(FinState),
}

pub struct RunLoop {
    // Consensus state.
    state: ClusterState,

    /// Mio pooler for asynchronous handling, aggregate events from consensus port and
    /// waker.
    poll: mio::Poll,
    /// Listener thread for MQTT connections from remote/local clients.
    listener: Listener,
    /// Ticker thread to periodically wake up other threads, defaul is 10ms.
    ticker: Ticker,
    /// Flusher thread for MQTT connections from remote/local clients.
    flusher: Flusher,
    /// Total number of shards within this node.
    shards: BTreeMap<u32, Shard>,

    /// Rebalancing algorithm.
    rebalancer: rebalance::Rebalancer,
    /// Index of subscribed topicfilters across all the sessions, local to this node.
    // TODO: Should we make this part of the ClusterState ?
    topic_filters: SubscribedTrie, // key=TopicFilter, val=(client_id, shard_id)
    /// Index of retained messages for each topic-name, across all the sessions, local
    /// to this node.
    // TODO: should we make this part of the ClusterState
    retained_messages: RetainedTrie,

    /// Back channel to communicate with application.
    app_tx: AppTx,
}

pub struct FinState {
    pub state: ClusterState,
    pub listener: Listener,
    pub ticker: Ticker,
    pub flusher: Flusher,
    pub shards: BTreeMap<u32, Shard>,
    pub topic_filters: SubscribedTrie,
    pub retained_messages: RetainedTrie,
}

impl Default for Cluster {
    fn default() -> Cluster {
        let config = Config::default();
        let mut def = Cluster {
            name: config.name.to_string(),
            prefix: String::default(),
            config,
            inner: Inner::Init,
        };
        def.prefix = def.prefix();
        def
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        use std::mem;

        let inner = mem::replace(&mut self.inner, Inner::Init);
        match inner {
            Inner::Init => debug!("{} drop ...", self.prefix),
            Inner::Handle(_waker, _thrd) => info!("{} drop ...", self.prefix),
            Inner::Tx(_waker, _tx) => info!("{} drop ...", self.prefix),
            Inner::Main(_run_loop) => info!("{} drop ...", self.prefix),
            Inner::Close(_fin_state) => info!("{} drop ...", self.prefix),
        }
    }
}

// Handle cluster
impl Cluster {
    /// Poll register token for waker event, OTP calls makde to this thread shall trigger
    /// this event.
    pub const TOKEN_WAKE: mio::Token = mio::Token(1);
    /// Poll register for consensus TcpStream.
    pub const TOKEN_CONSENSUS: mio::Token = mio::Token(2);

    /// Create a cluster from configuration. Cluster shall be in `Init` state. To start
    /// the cluster call [Cluster::spawn]
    pub fn from_config(config: Config) -> Result<Cluster> {
        // validate
        if config.num_shards() == 0 {
            err!(InvalidInput, desc: "num_shards can't be ZERO")?;
        } else if !util::is_power_of_2(config.num_shards()) {
            err!(
                InvalidInput,
                desc: "num. of shards must be power of 2 {}",
                config.num_shards()
            )?;
        }

        let mut val = Cluster {
            name: format!("{}-cluster-init", config.name),
            prefix: String::default(),
            config,
            inner: Inner::Init,
        };
        val.prefix = val.prefix();

        Ok(val)
    }

    pub fn spawn(self, node: Node, app_tx: AppTx) -> Result<Cluster> {
        use mio::Waker;

        if matches!(&self.inner, Inner::Handle(_, _) | Inner::Main(_)) {
            err!(InvalidInput, desc: "cluster can be spawned only in init-state ")?;
        }

        let poll = err!(IOError, try: mio::Poll::new(), "fail creating mio::Poll")?;
        let waker = Arc::new(Waker::new(poll.registry(), Self::TOKEN_WAKE)?);

        let rebalancer = rebalance::Rebalancer {
            config: self.config.clone(),
            algo: rebalance::Algorithm::SingleNode,
        };

        let state = {
            let topology = rebalancer.rebalance(&vec![node.clone()], vec![]);
            ClusterState::SingleNode {
                state: SingleNode { config: self.config.clone(), node, topology },
            }
        };

        let listener = Listener::default();
        let flusher = Flusher::from_config(self.config.clone())?.spawn(app_tx.clone())?;
        let shards = BTreeMap::default();

        let flusher_tx = flusher.to_tx();
        let topic_filters = SubscribedTrie::default();
        let retained_messages = RetainedTrie::default();
        let mut cluster = Cluster {
            name: format!("{}-cluster-main", self.config.name),
            prefix: String::default(),
            config: self.config.clone(),
            inner: Inner::Main(RunLoop {
                state,

                poll,
                listener,
                ticker: Ticker::default(),
                flusher,
                shards,

                rebalancer,
                topic_filters: topic_filters.clone(),
                retained_messages: retained_messages.clone(),

                app_tx: app_tx.clone(),
            }),
        };
        cluster.prefix = cluster.prefix();
        let mut thrd = Thread::spawn(&self.prefix, cluster);
        thrd.set_waker(Arc::clone(&waker));

        let mut cluster = Cluster {
            name: format!("{}-cluster-handle", self.config.name),
            prefix: String::default(),
            config: self.config.clone(),
            inner: Inner::Handle(waker, thrd),
        };
        cluster.prefix = cluster.prefix();

        {
            let mut ticker_shards = vec![];
            let mut shard_queues = BTreeMap::default();

            let mut shards = BTreeMap::default();
            for shard_id in 0..self.config.num_shards() {
                let (config, cluster_tx) = (self.config.clone(), cluster.to_tx());
                let shard = {
                    let args = crate::shard::SpawnArgs {
                        cluster: cluster_tx,
                        flusher: flusher_tx.to_tx(),
                        topic_filters: topic_filters.clone(),
                        retained_messages: retained_messages.clone(),
                    };
                    Shard::from_config(config, shard_id)?.spawn(args, app_tx.clone())?
                };

                shard_queues.insert(shard.shard_id, shard.to_msg_tx());
                ticker_shards.push(shard.to_tx());

                shards.insert(shard_id, shard);
            }

            for (_shard_id, shard) in shards.iter() {
                let iter = shard_queues.iter().map(|(id, s)| (*id, s.to_msg_tx()));
                let shard_queues = BTreeMap::from_iter(iter);
                shard.set_shard_queues(shard_queues)?;
            }

            let (config, clust_tx) = (self.config.clone(), cluster.to_tx());
            let listener = {
                let listener = Listener::from_config(config)?;
                listener.spawn(clust_tx, app_tx.clone())?
            };

            let ticker = Ticker::from_config(self.config.clone())?
                .spawn(ticker_shards, app_tx.clone())?;

            match &cluster.inner {
                Inner::Handle(_waker, thrd) => {
                    thrd.request(Request::Set { listener, ticker, shards })??;
                }
                _ => unreachable!(),
            }
        }

        Ok(cluster)
    }

    pub fn to_tx(&self) -> Self {
        info!("{} cloning tx ...", self.prefix);

        let inner = match &self.inner {
            Inner::Handle(waker, thrd) => Inner::Tx(Arc::clone(waker), thrd.to_tx()),
            Inner::Tx(waker, tx) => Inner::Tx(Arc::clone(waker), tx.clone()),
            _ => unreachable!(),
        };
        let mut val = Cluster {
            name: format!("{}-cluster-tx", self.config.name),
            prefix: String::default(),
            config: self.config.clone(),
            inner,
        };
        val.prefix = val.prefix();
        val
    }
}

pub enum Request {
    Set {
        listener: Listener,
        ticker: Ticker,
        shards: BTreeMap<u32, Shard>,
    },
    AddConnection(AddConnectionArgs),
    Close,
}

pub enum Response {
    Ok,
}

pub struct AddConnectionArgs {
    pub conn: mio::net::TcpStream,
    pub addr: net::SocketAddr,
    pub pkt: v5::Connect,
}

// calls to interface with cluster-thread.
impl Cluster {
    pub fn add_connection(&self, args: AddConnectionArgs) -> Result<()> {
        match &self.inner {
            Inner::Tx(_waker, tx) => tx.request(Request::AddConnection(args))??,
            _ => unreachable!(),
        };

        Ok(())
    }

    pub fn close_wait(mut self) -> Cluster {
        use std::mem;

        let inner = mem::replace(&mut self.inner, Inner::Init);
        match inner {
            Inner::Handle(_waker, thrd) => {
                thrd.request(Request::Close).ok();
                thrd.close_wait()
            }
            _ => unreachable!(),
        }
    }
}

impl Threadable for Cluster {
    type Req = Request;
    type Resp = Result<Response>;

    fn main_loop(mut self, rx: Rx<Self::Req, Self::Resp>) -> Self {
        info!(
            "{} spawn max_nodes:{} num_shards:{} ...",
            self.prefix,
            self.config.max_nodes(),
            self.config.num_shards(),
        );

        let mut events = Events::with_capacity(crate::POLL_EVENTS_SIZE);
        loop {
            let timeout: Option<time::Duration> = None;
            allow_panic!(&self, self.as_mut_poll().poll(&mut events, timeout));

            match self.mio_events(&rx, &events) {
                true /*disconnected*/ => break,
                false => (),
            };
        }

        self.handle_close(Request::Close);

        info!("{}, thread exit ...", self.prefix);

        self
    }
}

impl Cluster {
    // return (disconnected,)
    fn mio_events(&mut self, rx: &ThreadRx, events: &Events) -> bool {
        let mut count = 0_usize;
        let mut iter = events.iter();
        let disconnected = 'outer: loop {
            match iter.next() {
                Some(event) => {
                    trace!("{}, poll-event token:{}", self.prefix, event.token().0);
                    count += 1;

                    match event.token() {
                        Self::TOKEN_WAKE => loop {
                            // keep repeating until all control requests are drained
                            match self.drain_control_chan(rx) {
                                (_empty, true) => break 'outer true,
                                (true, _disconnected) => break,
                                (false, false) => (),
                            }
                        },
                        Self::TOKEN_CONSENSUS => todo!(),
                        _ => unreachable!(),
                    }
                }
                None => break false,
            }
        };

        debug!("{}, polled and got {} events", self.prefix, count);
        disconnected
    }

    // Return (empty, disconnected)
    // IPCFail,
    fn drain_control_chan(&mut self, rx: &ThreadRx) -> (bool, bool) {
        use crate::{thread::pending_requests, CONTROL_CHAN_SIZE};
        use Request::*;

        let (mut qs, empty, disconnected) = pending_requests(&rx, CONTROL_CHAN_SIZE);

        if matches!(&self.inner, Inner::Close(_)) {
            info!("{} skipping {} requests closed:true", self.prefix, qs.len());
            qs.drain(..);
        } else {
            debug!("{} process {} requests closed:false", self.prefix, qs.len());
        }

        // TODO: review control-channel handling for all threads. Should we panic or
        // return error.
        for q in qs.into_iter() {
            match q {
                (q @ Set { .. }, Some(tx)) => {
                    allow_panic!(&self, tx.send(Ok(self.handle_set(q))));
                }
                (q @ AddConnection(_), Some(tx)) => {
                    allow_panic!(&self, tx.send(Ok(self.handle_add_connection(q))));
                }
                (q @ Close, Some(tx)) => {
                    allow_panic!(&self, tx.send(Ok(self.handle_close(q))));
                }

                (_, _) => unreachable!(), // TODO: log meaning message.
            };
        }

        (empty, disconnected)
    }
}

// Main loop
impl Cluster {
    fn handle_set(&mut self, req: Request) -> Response {
        let run_loop = match &mut self.inner {
            Inner::Main(run_loop) => run_loop,
            _ => unreachable!(),
        };

        match req {
            Request::Set { listener, ticker, shards } => {
                run_loop.ticker = ticker;
                run_loop.listener = listener;
                run_loop.shards = shards;
            }
            _ => unreachable!(),
        }

        Response::Ok
    }

    // Errors - IPCFail,
    fn handle_add_connection(&mut self, req: Request) -> Response {
        use crate::shard::AddSessionArgs;

        let AddConnectionArgs { conn, addr, pkt: connect } = match req {
            Request::AddConnection(args) => args,
            _ => unreachable!(),
        };

        let RunLoop { shards, .. } = match &mut self.inner {
            Inner::Main(run_loop) => run_loop,
            _ => unreachable!(),
        };

        let client_id = connect.payload.client_id.clone();
        let shard_id = rebalance::Rebalancer::session_parition(
            &*client_id,
            self.config.num_shards(),
        );

        let shard = match shards.get_mut(&shard_id) {
            Some(shard) => shard,
            None => {
                // multi-node cluster, look at the topology and redirect client using
                // connack::server_reference, and close the connection.
                todo!()
            }
        };
        info!("{}, new connection {:?} mapped to shard {}", self.prefix, addr, shard_id);

        // Add session to the shard.
        allow_panic!(
            &self,
            shard.add_session(AddSessionArgs { conn, addr, pkt: connect })
        );

        Response::Ok
    }

    fn handle_close(&mut self, _: Request) -> Response {
        use std::mem;

        match mem::replace(&mut self.inner, Inner::Init) {
            Inner::Main(mut run_loop) => {
                info!("{}, closing shards:{}", self.prefix, run_loop.shards.len());

                mem::drop(run_loop.poll);

                let mut shards = BTreeMap::default();
                for (shard_id, shard) in run_loop.shards.into_iter() {
                    shards.insert(shard_id, shard.close_wait());
                }

                let listener = mem::replace(&mut run_loop.listener, Listener::default())
                    .close_wait();

                let ticker =
                    mem::replace(&mut run_loop.ticker, Ticker::default()).close_wait();

                let flusher =
                    mem::replace(&mut run_loop.flusher, Flusher::default()).close_wait();

                mem::drop(run_loop.rebalancer);

                let fin_state = FinState {
                    state: run_loop.state,
                    listener,
                    ticker,
                    flusher,
                    shards,
                    topic_filters: run_loop.topic_filters,
                    retained_messages: run_loop.retained_messages,
                };

                let _init = mem::replace(&mut self.inner, Inner::Close(fin_state));
                Response::Ok
            }
            Inner::Close(_) => Response::Ok,
            _ => unreachable!(),
        }
    }
}

impl Cluster {
    fn prefix(&self) -> String {
        format!("{}", self.name)
    }

    fn as_mut_poll(&mut self) -> &mut mio::Poll {
        match &mut self.inner {
            Inner::Main(RunLoop { poll, .. }) => poll,
            _ => unreachable!(),
        }
    }

    fn as_app_tx(&self) -> &mpsc::SyncSender<String> {
        match &self.inner {
            Inner::Main(RunLoop { app_tx, .. }) => app_tx,
            _ => unreachable!(),
        }
    }
}

/// Represents a Node in the cluster. `address` is the socket-address in which the
/// Node is listening for MQTT. Application must provide a valid address, other fields
/// like `weight` and `uuid` shall be assigned a meaningful default.
#[derive(Clone)]
pub struct Node {
    /// Unique id of the node.
    pub uuid: Uuid,
    /// Refer to [ConfigNode::path]
    pub path: path::PathBuf,
    /// Refer to [ConfigNode::weight]
    pub weight: u16,
    /// Refer to [ConfigNode::mqtt_address].
    pub mqtt_address: net::SocketAddr, // listen address
}

impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        self.uuid == other.uuid
    }
}

impl Eq for Node {}

impl Default for Node {
    fn default() -> Node {
        let config = ConfigNode::default();
        Node {
            mqtt_address: config.mqtt_address.clone(),
            path: config.path.clone(),
            weight: config.weight.unwrap(),
            uuid: config.uuid.unwrap().parse().unwrap(),
        }
    }
}

impl TryFrom<ConfigNode> for Node {
    type Error = Error;

    fn try_from(c: ConfigNode) -> Result<Node> {
        let node = Node::default();
        let uuid = match c.uuid.clone() {
            Some(uuid) => err!(InvalidInput, try: uuid.parse::<Uuid>())?,
            None => node.uuid,
        };

        let val = Node {
            mqtt_address: c.mqtt_address,
            path: c.path,
            weight: c.weight.unwrap_or(node.weight),
            uuid,
        };

        Ok(val)
    }
}

impl Hostable for Node {
    fn uuid(&self) -> uuid::Uuid {
        self.uuid
    }

    fn weight(&self) -> u16 {
        self.weight
    }

    fn path(&self) -> path::PathBuf {
        self.path.clone()
    }
}

pub enum ClusterState {
    /// Cluster is single-node.
    SingleNode { state: SingleNode },
    /// Cluster is in the process of updating its gods&nodes, and working out rebalance.
    Elastic { state: MultiNode },
    /// Cluster is stable.
    Stable { state: MultiNode },
}

pub struct MultiNode {
    config: Config,
    nodes: Vec<Node>, // TODO: should we split this into gods and nodes.
    topology: Vec<rebalance::Topology>, // list of shards mapped to node.
}

pub struct SingleNode {
    config: Config,
    node: Node,
    topology: Vec<rebalance::Topology>,
}

impl ClusterState {
    /// Return the list of shard-numbers that are hosted in this node.
    fn shards_in_node(&self, node: &Uuid) -> Vec<u32> {
        use ClusterState::*;

        let topology = match self {
            SingleNode { state } if node == &state.node.uuid => &state.topology,
            Stable { state } => &state.topology,
            _ => unreachable!(), // TODO: meaningful return.
        };
        topology.iter().filter(|t| node == &t.master.uuid).map(|t| t.shard).collect()
    }
}
