//! Contains main TCP handling.
//!
//! Relevant material for reading:
//! Main TCP rfc (skip if confident): https://tools.ietf.org/html/rfc793
//! Errata and comments: https://tools.ietf.org/html/rfc1122#section-4.2
//!     Notably still assuming some good-faith on hosts
//! TCP congestion control: https://tools.ietf.org/html/rfc5681
//! Attack avoidance: https://tools.ietf.org/html/rfc5961
//! Selective ACKs: https://tools.ietf.org/html/rfc2018
//! RST handling specifically: https://www.snellman.net/blog/archive/2016-02-01-tcp-rst/
//!     OS comparison in particular
use crate::layer::ip;
use crate::managed::{Map, SlotMap, slotmap::Key};
use crate::wire::{IpAddress, TcpPacket, TcpSeqNumber};
use crate::wire::PayloadMut;
use crate::time::{Duration, Expiration, Instant};

use super::connection::{
    Connection,
    Flow,
    Send,
    State,
    Receive};
use super::packet::{In, Raw};
use super::siphash::IsnGenerator;

/// Handles TCP connection states.
pub struct Endpoint<'a> {
    ports: Map<'a, FourTuple, Key>,
    states: SlotMap<'a, Slot>,
    isn_generator: IsnGenerator,
}

/// The TCP connection identifier, with four components.
///
/// In the model of TCP, there exists at most one bidirectional data stream for each unique tuple.
///
/// There is no inherent split into so called 'privileged' and unprivileged ports (the former often
/// refers to ports below 1024). This is mostly a best practise you *may* want to follow but it is
/// not built into the library.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FourTuple {
    /// The address identifying this machine/device for incoming segments.
    ///
    /// This address is not necessarily accurate (or unicast) until a connection has been
    /// established. In particular, it is used also for the netmask of a passively opened
    /// port and then modified when an incoming SYN is accepted.
    pub local: IpAddress,
    /// The address to which outgoing segments are sent.
    ///
    /// This is filled in for actively opened connections but contains the reserved all-zero
    /// address for a passively opened port until a connection attempt.
    pub remote: IpAddress,
    /// The local port of the connection.
    pub local_port: u16,
    /// The remote port of the connection.
    pub remote_port: u16,
}

/// A connection slot wrapping the internal connection state.
///
/// Contains the four-tuple which maps to the slot, completing the loop for lookups
/// (slotkey->4tuple,4tuple->slotkey). Usage of this acts similar to a slotmap where a dedicated
/// `SlotKey` allows referring to a connection outside of its lifetime without introducing
/// lifetime-tracked references and dependencies.
///
/// Internally used as the backing storage to open or accept a new connection.
#[derive(Clone, Copy, Debug, Hash)]
pub struct Slot {
    addr: FourTuple,
    connection: Connection,
}

/// The index of a connection.
///
/// Useful for storing in other structs to reference the connection at another point in time. Note
/// that the index will be invalidated when the connection itself is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotKey {
    key: Key,
}

/// An endpoint borrowed for receiving.
///
/// Incoming packets are matched against open ports and connections. Many parts of connection
/// state transitions are then performed automatically. If no direct answer was required the packet
/// becomes available for other uses.
pub struct Receiver<'a, 'e, H> {
    endpoint: Borrow<'a, 'e>,

    /// The upper protocol receiver.
    handler: H,
}

/// An endpoint borrowed for sending.
///
/// The send handler gets exclusive access the the internal state in order to create new active
/// connections or listening sockets. Each packet that becomes available for sending can be
/// committed to any of the open connections, or for creating or closing one as well.
pub struct Sender<'a, 'e, H> {
    endpoint: Borrow<'a, 'e>,

    /// The upper protocol receiver.
    handler: H,
}

struct Borrow<'a, 'e> {
    // TODO: could be immutable as well, just disallowing updates. Evaluate whether this is useful
    // or needed somewhere.
    inner: &'a mut Endpoint<'e>,
}

/// A partially mutable reference to a connection.
///
/// Retrieving an `Entry` for a given `SlotKey` consists of slicing the internal representation of
/// the `Endpoint` in such a way that it is possible to consistently change the four-tuple mapping
/// to that connection, to modify the connection itself, and to access the initial sequence number
/// generator.
pub struct Entry<'a> {
    // TODO: add remapping to the `Entry` based api of the map if required for performance.
    key: SlotKey,
    ports: &'a mut dyn PortMap,
    isn: &'a IsnGenerator,
    slot: &'a mut Slot,
}

/// A mutable reference to the key-structure of a connection.
///
/// From an `Entry` split into the connection and the keys referring to it in such a manner that
/// the keys can be edited without affecting the connection itself.
pub struct EntryKey<'a> {
    ports: &'a mut dyn PortMap,
    isn: &'a IsnGenerator,
    key_in_slot: &'a mut FourTuple,
}

/// Provides remapping a `SlotKey` under a different four tuple.
///
/// Erases the lifetime from the underlying `Map` itself.
pub(crate) trait PortMap {
    /// Note: does not permit failure so we must never expose it.
    fn remap(&mut self, old: FourTuple, new: FourTuple);
}

impl Endpoint<'_> {
    /// Returns a reference to the slot containing the corresponding connection.
    ///
    /// Can be used to inspect the TCP state and connection information.
    pub fn get(&self, index: SlotKey)
        -> Option<&Slot>
    {
        self.states.get(index.key)
    }

    /// Returns a mutable reference to the slot containing the corresponding connection.
    pub fn get_mut(&mut self, index: SlotKey)
        -> Option<&mut Slot>
    {
        self.states.get_mut(index.key)
    }

    /// Returns the entry of the corresponding connection.
    pub(crate) fn entry(&mut self, index: SlotKey)
        -> Option<Entry>
    {
        let slot = self.states.get_mut(index.key)?;

        Some(Entry {
            key: SlotKey { key: index.key },
            ports: &mut self.ports,
            isn: &mut self.isn_generator,
            slot,
        })
    }

    /// Returns the entry of a connection identification tuple.
    pub fn key_from_tuple(&mut self, tuple: FourTuple)
        -> Option<SlotKey>
    {
        let key = self.ports.get(&tuple).cloned()?;
        Some(SlotKey { key })
    }

    /// Returns the entry of a connection identification tuple.
    pub(crate) fn entry_from_tuple(&mut self, tuple: FourTuple)
        -> Option<Entry>
    {
        let key = self.key_from_tuple(tuple)?;
        self.entry(key)
    }

    /// Forcibly drop a connection.
    ///
    /// Note that this will *not* send any termination messages and *not* wait for a graceful exit
    /// period. It simply sets the state of the slot to `Closed` and removes all other mappings.
    pub fn remove(&mut self, index: SlotKey) {
        let addr = match self.get_mut(index) {
            Some(connection) => {
                connection.connection.change_state(State::Closed);
                connection.addr
            },
            None => return,
        };

        self.ports.entry(addr).remove();
        let _ = self.states.remove(index.key);
    }

    /// Opens a new port for listening.
    ///
    /// The source address is chosen during the first send operation on the create connection.
    /// Returns the key to use to inspect or modify the connection state and parameters.
    fn listen(&mut self, ip: IpAddress, port: u16)
        -> Option<SlotKey>
    {
        let key = FourTuple {
            local: ip,
            local_port: port,
            // Filled by the remote connection attempt.
            remote: IpAddress::Unspecified,
            remote_port: 0,
        };

        let (key, state) = self.create_state(key)?;
        state.connection.current = State::Listen;
        Some(key)
    }

    /// Actively try to connect to a remote TCP.
    ///
    /// This is not public as the caller controls the complete tuple.
    fn open(&mut self, tuple: FourTuple)
        -> Option<SlotKey>
    {
        let (key, _) = self.create_state(tuple)?;
        // Don't set to open yet, only after having sent the packet.
        Some(key)
    }

    fn create_state(&mut self, addr: FourTuple)
        -> Option<(SlotKey, &mut Slot)>
    {
        let connection = self.create_connection();

        let vacant = self.ports
            .entry(addr)
            .vacant()?;

        // FIXME: would be nicer to have an `Entry` api on the slotmap for peace of mind. It is
        // however mostly inconsequential right now.
        //
        // Reserves a slot, don't lose the key or we'd leak that reservation.
        let (key, slot) = self.states
            .reserve()?;

        slot.connection = connection;
        slot.addr = addr;
        vacant.insert(key);

        let key = SlotKey {
            key,
        };

        Some((key, slot))
    }

    /// Initialize a closed connection.
    ///
    /// The raw method is near useless, transition the connection to an appropriate state
    /// afterwards.
    fn create_connection(&mut self) -> Connection {
        Connection {
            current: State::Closed,
            previous: State::Closed,
            flow_control: Flow {
                congestion_window: 0,
                ssthresh: u32::max_value(),
                recover: TcpSeqNumber::default(),
            },
            receive_window: 0,
            sender_maximum_segment_size: 0,
            receiver_maximum_segment_size: 0,
            last_ack_receive_offset: TcpSeqNumber::default(),
            ack_timer: Expiration::Never,
            ack_timeout: Duration::from_millis(500),
            retransmission_timer: Instant::from_millis(0),
            retransmission_timeout: Duration::from_millis(3000),
            restart_timeout: Duration::from_millis(30000),
            selective_acknowledgements: false,
            duplicate_ack: 0,
            send: Send {
                unacked: TcpSeqNumber::default(),
                next: TcpSeqNumber::default(),
                last_time: Instant::from_millis(0),
                unsent: 0,
                window: 0,
                window_scale: 0,
                initial_seq: TcpSeqNumber::default(),
            },
            recv: Receive {
                acked: TcpSeqNumber::default(),
                next: TcpSeqNumber::default(),
                last_time: Instant::from_millis(0),
                window: 0,
                window_scale: 0,
                initial_seq: TcpSeqNumber::default(),
            },
        }
    }

    fn initial_seq_num(&mut self, id: FourTuple, time: Instant) -> TcpSeqNumber {
        self.isn_generator.get_isn(id, time)
    }
}

impl Slot {
    /// Return the connection four tuple of the slot.
    ///
    /// Note that the source or destination ports may be `0` if they have not been chosen yet. This
    /// happens when the connection has only been opened passively or no active SYN has been sent.
    pub fn four_tuple(&self) -> FourTuple {
        self.addr
    }

    /// Returns a reference to the connection contained in the slot.
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

impl<'ep> Endpoint<'ep> {
    /// Create a new endpoint.
    ///
    /// The map and states are **not** cleared.
    pub fn new(
        ports: Map<'ep, FourTuple, Key>,
        states: SlotMap<'ep, Slot>,
        isn_generator: IsnGenerator,
    ) -> Self {
        Endpoint {
            ports,
            states,
            isn_generator,
        }
    }

    /// Create a TCP receiver using this endpoint.
    pub fn recv<H>(&mut self, handler: H) -> Receiver<'_, 'ep, H> {
        Receiver { endpoint: self.borrow(), handler }
    }

    /// Create a TCP sender using this endpoint.
    pub fn send<H>(&mut self, handler: H) -> Sender<'_, 'ep, H> {
        Sender { endpoint: self.borrow(), handler }
    }

    fn borrow(&mut self) -> Borrow<'_, 'ep> {
        Borrow { inner: self, }
    }
}

impl<'a> Entry<'a> {
    /// Destructure into mapping metadata and a reference to the connection.
    pub fn into_key_value(self) -> (EntryKey<'a>, &'a mut Connection) {
        let entry_key = EntryKey {
            ports: self.ports,
            isn: self.isn,
            key_in_slot: &mut self.slot.addr,
        };

        let connection = &mut self.slot.connection;

        (entry_key, connection)
    }

    /// Get the slot key with which the entry was created.
    pub fn slot_key(&self) -> SlotKey {
        self.key
    }

    /// Get a mutable reference to the connection without destructuring.
    pub fn connection(&mut self) -> &mut Connection {
        &mut self.slot.connection
    }
}

impl EntryKey<'_> {
    /// Generate a new initial sequence number.
    pub fn initial_seq_num(&self, time: Instant) -> TcpSeqNumber {
        self.isn.get_isn(*self.key_in_slot, time)
    }

    pub fn four_tuple(&self) -> FourTuple {
        *self.key_in_slot
    }

    /// Move the connection state to a new connection tuple.
    ///
    /// # Panics
    /// `new` must not be taken by any other connection yet.
    pub fn set_four_tuple(&mut self, new: FourTuple) {
        self.ports.remap(*self.key_in_slot, new);
        *self.key_in_slot = new;
    }
}

#[cfg(test)]
impl<'a> EntryKey<'a> {
    pub(crate) fn fake(
        ports: &'a mut dyn PortMap,
        isn: &'a IsnGenerator,
        key_in_slot: &'a mut FourTuple,
    ) -> EntryKey<'a> {
        EntryKey { ports, isn, key_in_slot, }
    }
}

impl Default for Slot {
    fn default() -> Self {
       Slot {
           addr: FourTuple::default(),
           connection: Connection::zeroed(),
       }
    }
}

impl super::connection::Endpoint for Endpoint<'_> {
    fn get(&self, index: SlotKey) -> Option<&Slot> {
        Endpoint::get(self, index)
    }

    fn get_mut(&mut self, index: SlotKey) -> Option<&mut Slot> {
        Endpoint::get_mut(self, index)
    }

    fn remove(&mut self, index: SlotKey) {
        Endpoint::remove(self, index)
    }

    fn entry(&mut self, index: SlotKey) -> Option<Entry> {
        Endpoint::entry(self, index)
    }

    fn find_tuple(&mut self, tuple: FourTuple) -> Option<Entry> {
        if self.ports.entry(tuple).occupied().is_some() {
            Endpoint::entry_from_tuple(self, tuple)
        } else {
            Endpoint::entry_from_tuple(self, FourTuple {
                local: tuple.local,
                local_port: tuple.local_port,
                remote: IpAddress::Unspecified,
                remote_port: 0,
            })
        }
    }

    fn source_port(&mut self, _: IpAddress) -> Option<u16> {
        // FIXME: find a suitable source port....
        Some(80)
    }

    fn listen(&mut self, ip: IpAddress, port: u16) -> Option<SlotKey> {
        Endpoint::listen(self, ip, port)
    }

    fn open(&mut self, tuple: FourTuple) -> Option<SlotKey> {
        Endpoint::open(self, tuple)
    }

    fn initial_seq_num(&mut self, id: FourTuple, time: Instant) -> TcpSeqNumber {
        Endpoint::initial_seq_num(self, id, time)
    }
}

impl PortMap for Map<'_, FourTuple, Key> {
    fn remap(&mut self, old: FourTuple, new: FourTuple) {
        let old = self.entry(old)
            .occupied()
            // FIXME: unwrap justified? Seems like it may not.
            .unwrap();
        let value = *old.get();
        old.remove();

        self.entry(new)
            .vacant()
            // FIXME: nearly justified but how to ensure it was not mapped?
            .unwrap()
            .insert(value);
    }
}

impl<H, P> ip::Recv<P> for Receiver<'_, '_, H>
where
    P: PayloadMut,
    H: super::Recv<P>,
{
    fn receive(&mut self, ip_packet: ip::InPacket<P>) {
        let ip::InPacket { mut handle, packet } = ip_packet;

        let repr = packet.repr();
        let capabilities = handle.info().capabilities();
        let checksum = capabilities.tcp().rx_checksum(repr);

        let packet = match TcpPacket::new_checked(packet, checksum) {
            Ok(packet) => packet,
            // TODO: error logging.
            Err(_) => return (),
        };

        let arrived = match In::from_arriving(self.endpoint.inner, handle.borrow_mut(), packet) {
            Ok(arrived) => arrived,

            // TODO: error logging.
            Err(_) => return (),
        };

        self.handler.receive(arrived)
    }
}

impl<H, P> ip::Send<P> for Sender<'_, '_, H>
where
    P: PayloadMut,
    H: super::Send<P>,
{
    fn send(&mut self, ip_raw: ip::RawPacket<P>) {
        let ip::RawPacket { mut handle, payload } = ip_raw;

        let raw = Raw {
            ip: ip::RawPacket { handle: handle.borrow_mut(), payload },
            endpoint: self.endpoint.inner,
        };

        self.handler.send(raw)
    }
}
