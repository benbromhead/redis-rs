//! Redis cluster support.
//!
//! This module extends the library to be able to use cluster.
//! ClusterClient impletemts traits of ConnectionLike and Commands.
//!
//! Note that the cluster support currently does not provide pubsub
//! functionality.
//!
//! # Example
//! ```rust,no_run
//! use redis::Commands;
//! use redis::cluster::ClusterClient;
//!
//! let nodes = vec!["redis://127.0.0.1:6379/", "redis://127.0.0.1:6378/", "redis://127.0.0.1:6377/"];
//! let client = ClusterClient::open(nodes).unwrap();
//! let mut connection = client.get_connection().unwrap();
//!
//! let _: () = connection.set("test", "test_data").unwrap();
//! let rv: String = connection.get("test").unwrap();
//!
//! assert_eq!(rv, "test_data");
//! ```
//!
//! # Pipelining
//! ```rust,no_run
//! use redis::{Commands, pipe};
//! use redis::cluster::ClusterClient;
//!
//! let nodes = vec!["redis://127.0.0.1:6379/", "redis://127.0.0.1:6378/", "redis://127.0.0.1:6377/"];
//! let client = ClusterClient::open(nodes).unwrap();
//! let mut connection = client.get_connection().unwrap();
//!
//! let key = "test";
//!
//! let _: () = pipe()
//!     .rpush(key, "123").ignore()
//!     .ltrim(key, -10, -1).ignore()
//!     .expire(key, 60).ignore()
//!     .query(&mut connection).unwrap();
//! ```
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter::Iterator;
use std::thread;
use std::time::Duration;

use rand::{seq::{IteratorRandom, SliceRandom}, thread_rng, SeedableRng};

use super::{
    cmd, parse_redis_value, Cmd, ConnectionAddr, ConnectionInfo,
    ErrorKind, IntoConnectionInfo, RedisError, RedisResult, Value
};
use crate::aio::{Connection, ConnectionLike};
use crate::{RedisFuture, Pipeline};

use futures_util::{
    future::{Future, FutureExt, TryFutureExt},
    ready,
    sink::Sink,
    stream::{Stream, StreamExt, TryStreamExt as _},
};

use async_trait::async_trait;
use combine::lib::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::sync::atomic::Ordering;
use std::borrow::BorrowMut;
use rand::prelude::SmallRng;

const SLOT_SIZE: usize = 16384;

type SlotMap = BTreeMap<u16, String>;

/// This is a ClusterClientBuilder of Redis cluster client.

pub struct ClusterClientBuilder {
    initial_nodes: RedisResult<Vec<ConnectionInfo>>,
    readonly: bool,
    password: Option<String>,
    username: Option<String>,
}

impl ClusterClientBuilder {
    /// Generate the base configuration for new Client.
    pub fn new<T: IntoConnectionInfo>(initial_nodes: Vec<T>) -> ClusterClientBuilder {
        ClusterClientBuilder {
            initial_nodes: initial_nodes
                .into_iter()
                .map(|x| x.into_connection_info())
                .collect(),
            readonly: false,
            password: None,
            username: None,
        }
    }

    /// Connect to a redis cluster server and return a cluster client.
    /// This does not actually open a connection yet but it performs some basic checks on the URL.
    /// The password of initial nodes must be the same all.
    ///
    /// # Errors
    ///
    /// If it is failed to parse initial_nodes or the initial nodes has different password, an error is returned.
    pub async fn open(self) -> RedisResult<ClusterClient> {
        ClusterClient::open_internal(self)
    }

    /// Set password for new ClusterClient.
    pub fn password(mut self, password: String) -> ClusterClientBuilder {
        self.password = Some(password);
        self
    }

    /// Set username for new ClusterClient.
    pub fn username(mut self, username: String) -> ClusterClientBuilder {
        self.username = Some(username);
        self
    }

    /// Set read only mode for new ClusterClient.
    /// Default is not read only mode.
    /// When it is set to readonly mode, all query use replica nodes except there are no replica nodes.
    /// If there are no replica nodes, it use master node.
    pub fn readonly(mut self, readonly: bool) -> ClusterClientBuilder {
        self.readonly = readonly;
        self
    }
}

/// This is a Redis cluster client.
#[derive(Clone)]
pub struct ClusterClient {
    initial_nodes: Vec<ConnectionInfo>,
    readonly: bool,
    password: Option<String>,
    username: Option<String>,
}

impl ClusterClient {
    /// Connect to a redis cluster server and return a cluster client.
    /// This does not actually open a connection yet but it performs some basic checks on the URL.
    /// The password of initial nodes must be the same all.
    ///
    /// # Errors
    ///
    /// If it is failed to parse initial_nodes or the initial nodes has different password, an error is returned.
    pub async fn open<T: IntoConnectionInfo>(initial_nodes: Vec<T>) -> RedisResult<ClusterClient> {
        ClusterClientBuilder::new(initial_nodes).open().await
    }

    /// Open and get a Redis cluster connection.
    ///
    /// # Errors
    ///
    /// If it is failed to open connections and to create slots, an error is returned.
    pub async fn get_connection(&self) -> RedisResult<ClusterConnection> {
        ClusterConnection::new(
            self.initial_nodes.clone(),
            self.readonly,
            self.password.clone(),
            self.username.clone()
        ).await
    }

    fn open_internal(builder: ClusterClientBuilder) -> RedisResult<ClusterClient> {
        let initial_nodes = builder.initial_nodes?;
        let mut nodes = Vec::with_capacity(initial_nodes.len());
        let mut connection_info_password = None::<String>;
        let mut connection_info_username = None::<String>;

        for (index, info) in initial_nodes.into_iter().enumerate() {
            if let ConnectionAddr::Unix(_) = *info.addr {
                return Err(RedisError::from((ErrorKind::InvalidClientConfig,
                                             "This library cannot use unix socket because Redis's cluster command returns only cluster's IP and port.")));
            }

            if builder.password.is_none() {
                if index == 0 {
                    connection_info_password = info.passwd.clone();
                } else if connection_info_password != info.passwd {
                    return Err(RedisError::from((
                        ErrorKind::InvalidClientConfig,
                        "Cannot use different password among initial nodes.",
                    )));
                }
            }

            if builder.username.is_none() {
                if index == 0 {
                    connection_info_username = info.username.clone();
                } else if connection_info_username != info.username {
                    return Err(RedisError::from((
                        ErrorKind::InvalidClientConfig,
                        "Cannot use different username among initial nodes.",
                    )));
                }
            }

            nodes.push(info);
        }

        Ok(ClusterClient {
            initial_nodes: nodes,
            readonly: builder.readonly,
            password: builder.password.or(connection_info_password),
            username: builder.username.or(connection_info_username),
        })
    }
}

/// This is a connection of Redis cluster.
#[derive(Debug)]
pub struct ClusterConnection {
    initial_nodes: Vec<ConnectionInfo>,
    connections: Arc<Mutex<HashMap<String, Connection>>>,
    slots: Arc<Mutex<SlotMap>>,
    auto_reconnect: AtomicBool,
    readonly: bool,
    password: Option<String>,
    username: Option<String>,
    pub rng: SmallRng
}

impl ClusterConnection {
    async fn new(
        initial_nodes: Vec<ConnectionInfo>,
        readonly: bool,
        password: Option<String>,
        username: Option<String>,
    ) -> RedisResult<ClusterConnection> {
        let connections =
            Self::create_initial_connections(&initial_nodes, readonly, password.clone(), username.clone()).await?;
        let connection = ClusterConnection {
            initial_nodes,
            connections: Arc::new(Mutex::new(connections)),
            slots: Arc::new(Mutex::new(SlotMap::new())),
            auto_reconnect: AtomicBool::new(true),
            readonly,
            password,
            username,
            rng: SmallRng::from_entropy()
        };
        connection.refresh_slots().await?;

        Ok(connection)
    }

    /// Set an auto reconnect attribute.
    /// Default value is true;
    pub fn set_auto_reconnect(&self, value: bool) {
        self.auto_reconnect.store(value, Ordering::Relaxed);
    }

    /// Sets the write timeout for the connection.
    ///
    /// If the provided value is `None`, then `send_packed_command` call will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> RedisResult<()> {
        // let connections = self.connections.borrow();
        // for conn in connections.values() {
        //     conn.set_write_timeout(dur)?;
        // }

        //TODO wrap with timeout future
        Ok(())
    }

    /// Sets the read timeout for the connection.
    ///
    /// If the provided value is `None`, then `recv_response` call will
    /// block indefinitely. It is an error to pass the zero `Duration` to this
    /// method.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> RedisResult<()> {
        // let connections = self.connections.borrow();
        // for conn in connections.values() {
        //     conn.set_read_timeout(dur)?;
        // }

        //TODO wrap with timeout future

        Ok(())
    }

    /// Check that all connections it has are available (`PING` internally).
    pub async fn check_connection(&mut self) -> bool {
        for conn in self.connections.lock().await.values_mut() {
            if !check_connection(conn).await {
                return false;
            }
        }
        true
    }

    /// Returns the connection status.
    ///
    /// The connection is open until any `read_response` call recieved an
    /// invalid response from the server (most likely a closed or dropped
    /// connection, otherwise a Redis protocol error). When using unix
    /// sockets the connection is open until writing a command failed with a
    /// `BrokenPipe` error.

    async fn create_initial_connections(
        initial_nodes: &[ConnectionInfo],
        readonly: bool,
        password: Option<String>,
        username: Option<String>,
    ) -> RedisResult<HashMap<String, Connection>> {
        let mut connections = HashMap::with_capacity(initial_nodes.len());

        for info in initial_nodes.iter() {
            let addr = match *info.addr {
                ConnectionAddr::Tcp(ref host, port) => format!("redis://{}:{}", host, port),
                _ => panic!("No reach."),
            };

            if let Ok(mut conn) = connect(info.clone(), readonly, password.clone(), username.clone()).await {
                if check_connection(&mut conn).await {
                    connections.insert(addr, conn);
                    break;
                }
            }
        }

        if connections.is_empty() {
            return Err(RedisError::from((
                ErrorKind::IoError,
                "It is failed to check startup nodes.",
            )));
        }
        Ok(connections)
    }

    // Query a node to discover slot-> master mappings.
    async fn refresh_slots(&self) -> RedisResult<()> {
        let mut slots = self.slots.lock().await;
        *slots = if self.readonly {
            let mut rng = self.rng.clone();
            self.create_new_slots(|slot_data| {
                let replicas = slot_data.replicas();
                if replicas.is_empty() {
                    slot_data.master().to_string()
                } else {
                    replicas.choose(&mut rng).unwrap().to_string()
                }
            }).await?
        } else {
            self.create_new_slots(|slot_data| slot_data.master().to_string()).await?
        };

        let mut connections = self.connections.lock().await;
        *connections = {
            // Remove dead connections and connect to new nodes if necessary
            let mut new_connections = HashMap::with_capacity(connections.len());

            for addr in slots.values() {
                if !new_connections.contains_key(addr) {
                    if connections.contains_key(addr) {
                        let mut conn = connections.remove(addr).unwrap();
                        if check_connection(&mut conn).await {
                            new_connections.insert(addr.to_string(), conn);
                            continue;
                        }
                    }

                    if let Ok(mut conn) =
                    connect(addr.as_ref(), self.readonly, self.password.clone(), self.username.clone()).await
                    {
                        if check_connection(&mut conn).await {
                            new_connections.insert(addr.to_string(), conn);
                        }
                    }
                }
            }
            new_connections
        };

        Ok(())
    }

    async fn create_new_slots<F>(&self, mut get_addr: F) -> RedisResult<SlotMap>
        where
            F: FnMut(&Slot) -> String,
    {
        let mut connections = self.connections.lock().await;
        let mut new_slots = None;
        let mut rng = self.rng.clone();
        let len = connections.len();
        let mut samples = connections.values_mut().choose_multiple(&mut rng, len);

        for mut conn in samples.iter_mut() {
            if let Ok(mut slots_data) = get_slots(&mut conn).await {
                slots_data.sort_by_key(|s| s.start());
                let last_slot = slots_data.iter().try_fold(0, |prev_end, slot_data| {
                    if prev_end != slot_data.start() {
                        return Err(RedisError::from((
                            ErrorKind::ResponseError,
                            "Slot refresh error.",
                            format!(
                                "Received overlapping slots {} and {}..{}",
                                prev_end, slot_data.start, slot_data.end
                            ),
                        )));
                    }
                    Ok(slot_data.end() + 1)
                })?;

                if usize::from(last_slot) != SLOT_SIZE {
                    return Err(RedisError::from((
                        ErrorKind::ResponseError,
                        "Slot refresh error.",
                        format!("Lacks the slots >= {}", last_slot),
                    )));
                }

                new_slots = Some(
                    slots_data
                        .iter()
                        .map(|slot_data| (slot_data.end(), get_addr(slot_data)))
                        .collect(),
                );
                break;
            }
        }

        match new_slots {
            Some(new_slots) => Ok(new_slots),
            None => Err(RedisError::from((
                ErrorKind::ResponseError,
                "Slot refresh error.",
                "didn't get any slots from server".to_string(),
            ))),
        }
    }



    async fn get_connection<'a>(
        &'a self,
        connections: &'a mut HashMap<String, Connection>,
        slot: u16,
    ) -> RedisResult<(String, &'a mut Connection)> {
        let slots = self.slots.lock().await;
        if let Some((_, addr)) = slots.range(&slot..).next() {
            Ok((
                addr.to_string(),
                self.get_connection_by_addr(connections, addr).await?,
            ))
        } else {
            // try a random node next.  This is safe if slots are involved
            // as a wrong node would reject the request.
            Ok(get_random_connection(connections, None, &self.rng))
        }
    }


    pub async fn get_connection_string(
        &self,
        slot: u16,
    ) -> Option<String> {
        let slots = self.slots.lock().await;
        if let Some((_, addr)) = slots.range(&slot..).next() {
            Some(addr.to_string())
        } else {
            None
        }
    }

    async fn get_connection_by_addr<'a>(
        &self,
        connections: &'a mut HashMap<String, Connection>,
        addr: &str,
    ) -> RedisResult<&'a mut Connection> {
        if connections.contains_key(addr) {
            Ok(connections.get_mut(addr).unwrap())
        } else {
            // Create new connection.
            // TODO: error handling
            let conn = connect(addr, self.readonly, self.password.clone(), self. username.clone()).await?;
            Ok(connections.entry(addr.to_string()).or_insert(conn))
        }
    }
}

trait MergeResults {
    fn merge_results(_values: HashMap<&str, Self>) -> Self
        where
            Self: Sized;

    fn merge_results_pipe(_values: HashMap<&str, Vec<Self>>) -> Self
        where
            Self: Sized;
}
//
// impl MergeResults for Value {
//     fn merge_results(values: HashMap<&str, Value>) -> Value {
//         let mut items = vec![];
//         for (addr, value) in values.into_iter() {
//             items.push(Value::Bulk(vec![
//                 Value::Data(addr.as_bytes().to_vec()),
//                 value,
//             ]));
//         }
//         Value::Bulk(items)
//     }
//
//     fn merge_results_pipe(_values: HashMap<&str, Vec<Value>>) -> Vec<Value> {
//         unreachable!("attempted to merge a pipeline. This should not happen");
//     }
// }


#[async_trait]
impl ConnectionLike for ClusterConnection {
    async fn req_packed_command(& mut self, cmd: & Cmd) -> RedisResult<Value> {
        let slot = match RoutingInfo::for_packed_command(cmd) {
            Some(RoutingInfo::Random) => None,
            Some(RoutingInfo::Slot(slot)) => Some(slot),
            Some(RoutingInfo::AllNodes) | Some(RoutingInfo::AllMasters) => {
                let mut connections = self.connections.lock().await;
                let mut results = HashMap::new();

                // TODO: reconnect and shit
                for (addr, connection) in connections.iter_mut() {
                    results.insert(addr.as_str(), connection.req_packed_command(cmd).await?);
                }

                let mut items = vec![];
                for (addr, value) in results.into_iter() {
                    items.push(Value::Bulk(vec![
                        Value::Data(addr.as_bytes().to_vec()),
                        value,
                    ]));
                }
                return Ok(Value::Bulk(items));

            }
            None => {
                return Err((
                    ErrorKind::ClientError,
                    "this command cannot be safely routed in cluster mode",
                )
                    .into())
            }
        };

        let mut retries = 16;
        let mut excludes = HashSet::new();
        let mut asking = None::<String>;
        loop {
            // Get target address and response.
            let (addr, rv) = {
                let mut connections = self.connections.lock().await;
                let (addr, conn) = if let Some(addr) = asking.take() {
                    let conn = self.get_connection_by_addr(&mut *connections, &addr).await?;
                    // if we are in asking mode we want to feed a single
                    // ASKING command into the connection before what we
                    // actually want to execute.
                    let askcmd = crate::cmd("ASKING");
                    conn.req_packed_command(&askcmd).await?;
                    (addr.to_string(), conn)
                } else if !excludes.is_empty() || slot.is_none() {
                    get_random_connection(&mut *connections, Some(&excludes), &self.rng)
                } else {
                    self.get_connection(&mut *connections, slot.unwrap()).await?
                };
                (addr, conn.req_packed_command(cmd).await)
            };

            match rv {
                Ok(rv) => return Ok(rv),
                Err(err) => {
                    retries -= 1;
                    if retries == 0 {
                        return Err(err);
                    }

                    if err.is_cluster_error() {
                        let kind = err.kind();

                        if kind == ErrorKind::Ask {
                            asking = err.redirect_node().map(|x| x.0.to_string());
                        } else if kind == ErrorKind::Moved {
                            // Refresh slots and request again.
                            self.refresh_slots().await?;
                            excludes.clear();
                            continue;
                        } else if kind == ErrorKind::TryAgain || kind == ErrorKind::ClusterDown {
                            // Sleep and retry.
                            let sleep_time = 2u64.pow(16 - retries.max(9)) * 10;
                            thread::sleep(Duration::from_millis(sleep_time));
                            excludes.clear();
                            continue;
                        }
                    } else if self.auto_reconnect.fetch_and(true, Ordering::Relaxed) && err.is_io_error() {
                        let new_connections = Self::create_initial_connections(
                            &self.initial_nodes,
                            self.readonly,
                            self.password.clone(),
                            self.username.clone(),
                        ).await?;
                        {
                            let mut connections = self.connections.lock().await;
                            *connections = new_connections;
                        }
                        self.refresh_slots().await?;
                        excludes.clear();
                        continue;
                    } else {
                        return Err(err);
                    }

                    excludes.insert(addr);

                    let connections = self.connections.lock().await;
                    if excludes.len() >= connections.len() {
                        return Err(err);
                    }
                }
            }
        }
    }

    async fn req_packed_commands(
        & mut self,
        cmd: &crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisResult<Vec<Value>> {
        let slot = match RoutingInfo::for_packed_command_bytes(&cmd.get_packed_pipeline()) {
            Some(RoutingInfo::Random) => None,
            Some(RoutingInfo::Slot(slot)) => Some(slot),
            Some(RoutingInfo::AllNodes) | Some(RoutingInfo::AllMasters) => {

                let mut connections = self.connections.lock().await;
                let mut results = HashMap::new();

                // TODO: reconnect and shit
                for (addr, connection) in connections.iter_mut() {
                    results.insert(addr.as_str(), connection.req_packed_commands(cmd, offset, count).await?);
                }

                return Ok(results.values().flatten().cloned().collect());
            }
            None => {
                return Err((
                    ErrorKind::ClientError,
                    "this command cannot be safely routed in cluster mode",
                )
                    .into())
            }
        };

        let mut retries = 16;
        let mut excludes = HashSet::new();
        let mut asking = None::<String>;
        loop {
            // Get target address and response.
            let (addr, rv) = {
                let mut connections = self.connections.lock().await;
                let (addr, conn) = if let Some(addr) = asking.take() {
                    let conn = self.get_connection_by_addr(&mut *connections, &addr).await?;
                    // if we are in asking mode we want to feed a single
                    // ASKING command into the connection before what we
                    // actually want to execute.
                    let askcmd = crate::cmd("ASKING");
                    conn.req_packed_command(&askcmd).await?;
                    (addr.to_string(), conn)
                } else if !excludes.is_empty() || slot.is_none() {
                    get_random_connection(&mut *connections, Some(&excludes), &self.rng)
                } else {
                    self.get_connection(&mut *connections, slot.unwrap()).await?
                };
                (addr, conn.req_packed_commands_with_errors(cmd, offset, count).await)
            };

            match rv {
                Ok(rv) => {
                    let intermediate: Result<Vec<_>, _> = rv.into_iter().map(|r| r).collect();
                    return intermediate;
                },
                Err(err) => {
                    retries -= 1;
                    if retries == 0 {
                        return Err(err);
                    }

                    if err.is_cluster_error() {
                        let kind = err.kind();

                        if kind == ErrorKind::Ask {
                            asking = err.redirect_node().map(|x| x.0.to_string());
                        } else if kind == ErrorKind::Moved {
                            // Refresh slots and request again.
                            self.refresh_slots().await?;
                            excludes.clear();
                            continue;
                        } else if kind == ErrorKind::TryAgain || kind == ErrorKind::ClusterDown {
                            // Sleep and retry.
                            let sleep_time = 2u64.pow(16 - retries.max(9)) * 10;
                            thread::sleep(Duration::from_millis(sleep_time));
                            excludes.clear();
                            continue;
                        }
                    } else if self.auto_reconnect.fetch_and(true, Ordering::Relaxed) && err.is_io_error() {
                        let new_connections = Self::create_initial_connections(
                            &self.initial_nodes,
                            self.readonly,
                            self.password.clone(),
                            self.username.clone()
                        ).await?;
                        {
                            let mut connections = self.connections.lock().await;
                            *connections = new_connections;
                        }
                        self.refresh_slots().await?;
                        excludes.clear();
                        continue;
                    } else {
                        return Err(err);
                    }

                    excludes.insert(addr);

                    let connections = self.connections.lock().await;
                    if excludes.len() >= connections.len() {
                        return Err(err);
                    }
                }
            }
        }
    }

    fn get_db(&self) -> i64 {
        0
    }

    async fn req_packed_commands_raw(&mut self, cmd: &Pipeline, offset: usize, count: usize) -> RedisResult<Vec<RedisResult<Value>>> {

        let slot = match RoutingInfo::for_packed_command_bytes(&cmd.get_packed_pipeline()) {
            Some(RoutingInfo::Random) => None,
            Some(RoutingInfo::Slot(slot)) => Some(slot),
            Some(RoutingInfo::AllNodes) | Some(RoutingInfo::AllMasters) => {

                let mut connections = self.connections.lock().await;
                let mut results = HashMap::new();

                // TODO: reconnect and shit
                for (addr, connection) in connections.iter_mut() {
                    results.insert(addr.as_str(), connection.req_packed_commands_with_errors(cmd, offset, count).await?);
                }

                return Ok(results.values().flatten().cloned().collect());
            }
            None => {
                return Err((
                    ErrorKind::ClientError,
                    "this command cannot be safely routed in cluster mode",
                )
                    .into())
            }
        };

        let mut retries = 16;
        let mut excludes = HashSet::new();
        let mut asking = None::<String>;
        loop {
            // Get target address and response.
            let (addr, rv) = {
                let mut connections = self.connections.lock().await;
                let (addr, conn) = if let Some(addr) = asking.take() {
                    let conn = self.get_connection_by_addr(&mut *connections, &addr).await?;
                    // if we are in asking mode we want to feed a single
                    // ASKING command into the connection before what we
                    // actually want to execute.
                    let askcmd = crate::cmd("ASKING");
                    conn.req_packed_command(&askcmd).await?;
                    (addr.to_string(), conn)
                } else if !excludes.is_empty() || slot.is_none() {
                    get_random_connection(&mut *connections, Some(&excludes), &self.rng)
                } else {
                    self.get_connection(&mut *connections, slot.unwrap()).await?
                };
                (addr, conn.req_packed_commands_with_errors(cmd, offset, count).await)
            };

            match rv {
                Ok(rv) => {
                    return Ok(rv);
                },
                Err(err) => {
                    retries -= 1;
                    if retries == 0 {
                        return Err(err);
                    }

                    if err.is_cluster_error() {
                        let kind = err.kind();

                        if kind == ErrorKind::Ask {
                            asking = err.redirect_node().map(|x| x.0.to_string());
                        } else if kind == ErrorKind::Moved {
                            // Refresh slots and request again.
                            self.refresh_slots().await?;
                            excludes.clear();
                            continue;
                        } else if kind == ErrorKind::TryAgain || kind == ErrorKind::ClusterDown {
                            // Sleep and retry.
                            let sleep_time = 2u64.pow(16 - retries.max(9)) * 10;
                            thread::sleep(Duration::from_millis(sleep_time));
                            excludes.clear();
                            continue;
                        }
                    } else if self.auto_reconnect.fetch_and(true, Ordering::Relaxed) && err.is_io_error() {
                        let new_connections = Self::create_initial_connections(
                            &self.initial_nodes,
                            self.readonly,
                            self.password.clone(),
                            self.username.clone()
                        ).await?;
                        {
                            let mut connections = self.connections.lock().await;
                            *connections = new_connections;
                        }
                        self.refresh_slots().await?;
                        excludes.clear();
                        continue;
                    } else {
                        return Err(err);
                    }

                    excludes.insert(addr);

                    let connections = self.connections.lock().await;
                    if excludes.len() >= connections.len() {
                        return Err(err);
                    }
                }
            }
        }
    }
}

// impl Clone for ClusterClient {
//     fn clone(&self) -> ClusterClient {
//         ClusterClient::open(self.initial_nodes.clone()).await.unwrap()
//     }
// }

async fn connect<T: IntoConnectionInfo>(
    info: T,
    readonly: bool,
    password: Option<String>,
    username: Option<String>,
) -> RedisResult<Connection>
    where
        T: std::fmt::Debug,
{
    let mut connection_info = info.into_connection_info()?;
    connection_info.passwd = password;
    connection_info.username = username;
    let client = super::Client::open(connection_info)?;

    let mut con = client.get_async_connection().await?;
    if readonly {
        cmd("READONLY").query_async(&mut con).await?;
    }
    Ok(con)
}

async fn check_connection(conn: &mut Connection) -> bool {
    let mut cmd = Cmd::new();
    cmd.arg("PING");
    cmd.query_async::<_, String>(conn).await.is_ok()
}

fn get_random_connection<'a>(
    connections: &'a mut HashMap<String, Connection>,
    excludes: Option<&'a HashSet<String>>,
    rng: &'a SmallRng
) -> (String, &'a mut Connection) {
    let mut rng= rng.clone();
    let addr = match excludes {
        Some(excludes) if excludes.len() < connections.len() => connections
            .keys()
            .filter(|key| !excludes.contains(*key))
            .choose(& mut rng)
            .unwrap()
            .to_string(),
        _ => connections.keys().choose(& mut rng).unwrap().to_string(),
    };

    let con = connections.get_mut(&addr).unwrap();
    (addr, con)
}

fn get_hashtag(key: &[u8]) -> Option<&[u8]> {
    let open = key.iter().position(|v| *v == b'{');
    let open = match open {
        Some(open) => open,
        None => return None,
    };

    let close = key[open..].iter().position(|v| *v == b'}');
    let close = match close {
        Some(close) => close,
        None => return None,
    };

    let rv = &key[open + 1..open + close];
    if rv.is_empty() {
        None
    } else {
        Some(rv)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RoutingInfo {
    AllNodes,
    AllMasters,
    Random,
    Slot(u16),
}

fn get_arg(values: &[Value], idx: usize) -> Option<&[u8]> {
    match values.get(idx) {
        Some(Value::Data(ref data)) => Some(&data[..]),
        _ => None,
    }
}

fn get_command_arg(values: &[Value], idx: usize) -> Option<Vec<u8>> {
    get_arg(values, idx).map(|x| x.to_ascii_uppercase())
}

fn get_u64_arg(values: &[Value], idx: usize) -> Option<u64> {
    get_arg(values, idx)
        .and_then(|x| std::str::from_utf8(x).ok())
        .and_then(|x| x.parse().ok())
}

impl RoutingInfo {
    pub fn for_packed_command(cmd: &Cmd) -> Option<RoutingInfo> {
        parse_redis_value(&cmd.get_packed_command()).ok().and_then(RoutingInfo::for_value)
    }

    pub fn for_packed_command_bytes(cmd: &Vec<u8>) -> Option<RoutingInfo> {
        parse_redis_value(cmd).ok().and_then(RoutingInfo::for_value)
    }

    pub fn for_value(value: Value) -> Option<RoutingInfo> {
        let args = match value {
            Value::Bulk(args) => args,
            _ => return None,
        };

        match &get_command_arg(&args, 0)?[..] {
            b"FLUSHALL" | b"FLUSHDB" | b"SCRIPT" | b"ACL" => Some(RoutingInfo::AllMasters),
            b"ECHO" | b"CONFIG" | b"CLIENT" | b"SLOWLOG" | b"DBSIZE" | b"LASTSAVE" | b"PING"
            | b"INFO" | b"BGREWRITEAOF" | b"BGSAVE" | b"CLIENT LIST" | b"SAVE" | b"TIME"
            | b"KEYS" | b"AUTH" => Some(RoutingInfo::AllNodes),
            b"SCAN" | b"CLIENT SETNAME" | b"SHUTDOWN" | b"SLAVEOF" | b"REPLICAOF"
            | b"SCRIPT KILL" | b"MOVE" | b"BITOP" => None,
            b"EVALSHA" | b"EVAL" => {
                let key_count = get_u64_arg(&args, 2)?;
                if key_count == 0 {
                    Some(RoutingInfo::Random)
                } else {
                    get_arg(&args, 3).and_then(RoutingInfo::for_key)
                }
            }
            b"XGROUP" | b"XINFO" => get_arg(&args, 2).and_then(RoutingInfo::for_key),
            b"XREAD" | b"XREADGROUP" => {
                let streams_position = args.iter().position(|a| match a {
                    Value::Data(a) => a == b"STREAMS",
                    _ => false,
                })?;
                get_arg(&args, streams_position + 1).and_then(RoutingInfo::for_key)
            }
            _ => match get_arg(&args, 1) {
                Some(key) => RoutingInfo::for_key(key),
                None => Some(RoutingInfo::Random),
            },
        }
    }

    pub fn for_key(key: &[u8]) -> Option<RoutingInfo> {
        let key = match get_hashtag(&key) {
            Some(tag) => tag,
            None => &key,
        };
        Some(RoutingInfo::Slot(
            crc16::State::<crc16::XMODEM>::calculate(key) % SLOT_SIZE as u16,
        ))
    }
}

#[derive(Debug)]
struct Slot {
    start: u16,
    end: u16,
    master: String,
    replicas: Vec<String>,
}

impl Slot {
    pub fn start(&self) -> u16 {
        self.start
    }
    pub fn end(&self) -> u16 {
        self.end
    }
    pub fn master(&self) -> &str {
        &self.master
    }
    #[allow(dead_code)]
    pub fn replicas(&self) -> &Vec<String> {
        &self.replicas
    }
}

// Get slot data from connection.
async fn get_slots(connection: &mut Connection) -> RedisResult<Vec<Slot>> {
    let mut cmd = Cmd::new();
    cmd.arg("CLUSTER").arg("SLOTS");
    let value = connection.req_packed_command(&cmd).await?;

    // Parse response.
    let mut result = Vec::with_capacity(2);

    if let Value::Bulk(items) = value {
        let mut iter = items.into_iter();
        while let Some(Value::Bulk(item)) = iter.next() {
            if item.len() < 3 {
                continue;
            }

            let start = if let Value::Int(start) = item[0] {
                start as u16
            } else {
                continue;
            };

            let end = if let Value::Int(end) = item[1] {
                end as u16
            } else {
                continue;
            };

            let mut nodes: Vec<String> = item
                .into_iter()
                .skip(2)
                .filter_map(|node| {
                    if let Value::Bulk(node) = node {
                        if node.len() < 2 {
                            return None;
                        }

                        let ip = if let Value::Data(ref ip) = node[0] {
                            String::from_utf8_lossy(ip)
                        } else {
                            return None;
                        };
                        if ip.is_empty() {
                            return None;
                        }

                        let port = if let Value::Int(port) = node[1] {
                            port
                        } else {
                            return None;
                        };
                        Some(format!("redis://{}:{}", ip, port))
                    } else {
                        None
                    }
                })
                .collect();

            if nodes.is_empty() {
                continue;
            }

            let replicas = nodes.split_off(1);
            result.push(Slot {
                start,
                end,
                master: nodes.pop().unwrap(),
                replicas,
            });
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::get_hashtag;
    use super::{ClusterClient, ClusterClientBuilder};
    use super::{ConnectionInfo, IntoConnectionInfo};

    fn get_connection_data() -> Vec<ConnectionInfo> {
        vec![
            "redis://127.0.0.1:6379".into_connection_info().unwrap(),
            "redis://127.0.0.1:6378".into_connection_info().unwrap(),
            "redis://127.0.0.1:6377".into_connection_info().unwrap(),
        ]
    }

    fn get_connection_data_with_password() -> Vec<ConnectionInfo> {
        vec![
            "redis://:password@127.0.0.1:6379"
                .into_connection_info()
                .unwrap(),
            "redis://:password@127.0.0.1:6378"
                .into_connection_info()
                .unwrap(),
            "redis://:password@127.0.0.1:6377"
                .into_connection_info()
                .unwrap(),
        ]
    }

    #[tokio::test(threaded_scheduler)]
    async fn give_no_password() {
        let client = ClusterClient::open(get_connection_data()).await.unwrap();
        assert_eq!(client.password, None);
    }

    #[tokio::test(threaded_scheduler)]
    async fn give_password_by_initial_nodes() {
        let client = ClusterClient::open(get_connection_data_with_password()).await.unwrap();
        assert_eq!(client.password, Some("password".to_string()));
    }

    #[tokio::test(threaded_scheduler)]
    async fn give_different_password_by_initial_nodes() {
        let result = ClusterClient::open(vec![
            "redis://:password1@127.0.0.1:6379",
            "redis://:password2@127.0.0.1:6378",
            "redis://:password3@127.0.0.1:6377",
        ]).await;
        assert!(result.is_err());
    }

    #[tokio::test(threaded_scheduler)]
    async fn give_password_by_method() {
        let client = ClusterClientBuilder::new(get_connection_data_with_password())
            .password("pass".to_string())
            .open().await
            .unwrap();
        assert_eq!(client.password, Some("pass".to_string()));
    }

    #[tokio::test(threaded_scheduler)]
    async fn test_get_hashtag() {
        assert_eq!(get_hashtag(&b"foo{bar}baz"[..]), Some(&b"bar"[..]));
        assert_eq!(get_hashtag(&b"foo{}{baz}"[..]), None);
        assert_eq!(get_hashtag(&b"foo{{bar}}zap"[..]), Some(&b"{bar"[..]));
    }
}
