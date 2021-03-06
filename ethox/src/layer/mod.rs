//! The process logic of protocol layers.
//!
//! This is not a strict OSI stack but rather a group of logical modules to provide a set of
//! intertwined protocols. For example note that the `arp` functionality is also integrated into
//! the `ip` layer as it is required for using the ethernet layer below.
//!
//! ## Layering
//!
//! Each protocol layer is split into two parts; the packet logic contained in `wire` and the
//! processing part in this module. An endpoint represents the local state of a protocol. This
//! state can be used to process packets of that layer. The state is open to modifications as part
//! of a user program while processing does not take place, similar to reconfiguration on the OS
//! level with utilities such as `arp`, `ifconfig`, etc.
//!
//! The general structure of each layer is very similar:
//!
//! * Three types of packet encapsulation: In, Raw, and Out. The first represents an incoming
//!   packet with supported features. The second is a packet buffer that can be initialized utilizing
//!   the network layers below. And the last is an initialized packet that can be sent outwards.
//!
//!   ```text
//!   Raw --init-->Out
//!    ^           ^|
//!    |     reinit||into_in
//!    |           ||
//!    \           |v
//!     \--deinit--In
//!   ```
//!
//! * An endpoint component describing the persistent data of a Host on that layer. A receiver and
//!   sender can then make use of the layer by borrowing it while supplying the handler for the
//!   next upper layer.
//!
//! ## Receiving
//!
//! Many layer implementations process packets by routing them to layers conceptually above them.
//! This functionality is provided via abstract traits accepting the processed packets of that
//! layer which contain the payload to-be-consumed in the layer above. The encapsulation could be
//! removed if the upper layer does not require any knowledge of the layer below. However, it must
//! be preserved when one wants to use the lower layer for device or protocol specific actions.
//!
//! ## Sending
//!
//! Layers that are capable of send operations (in the sense of packet sending, not logical streams
//! such as TCP) provide a trait that processes Raw packet representations. Initialize the empty
//! packet buffer with data of the layer -- destination address in the case of ip -- and some
//! metadata about the payload that you want to emplace -- most often just the length. Then the RAw
//! packet is converted into an Out packet which offers methods to set the payload while having an
//! initialized and immutable header structure. Finally after inserting the desired payload,
//! request to send the Out packet which will finalize header fields such as checksums and queue
//! the packet buffer in the underlying NIC.
//!
//! ## Answering
//!
//! Many packets require a specified response from a particular layer. With the performance goal in
//! mind but under the constraint of memory allocation we may want to utilize the already valid
//! packet data to construct such an answer in-place of the just received packet. This is important
//! especially for icmp pings and routing functionality. There are two ways to avoid copying the
//! data in that case:
//!
//! * Allocate an additional packet buffer. The extreme of this option allows arbitrary buffer
//!   allocation and owning by the user's code which is seldom fit or even possible in resource
//!   constrained environments. Since buffers then become a contended resource this creates several
//!   DOS risks as well as buffer bloat.
//! * Reinitialize the packet header structures in-place while avoiding to write to any of the
//!   payload. In particular each layer calculates the required new length to which the final layer
//!   resizes the buffer while ensuring the outer payload is shifted into its new position. Then
//!   each layer can emit its representation again into the appropriate place. This is what `ethox`
//!   tries to do and should avoid any shifts if the new headers have the same size as the already
//!   existing ones.
//!
//! ## In-depth packet representation
//!
//! These are the design goals:
//! * Packet encapsulations may have internal invariants. In particular, the design must not depend
//!   on particular implementation of `AsRef<[u8]> + AsMut<[u8]>` to allow this. Thus, the
//!   processing pipeline must be able to store a reference to the packet content whose lifetime
//!   does not restrict access to other relevant data elsewhere. This needs to be cleanly
//!   separated.
//! * Minimize the number of 'callback' arguments, and avoid double dispatch. Single dispatch is
//!   okay, and the arguments that it receives should provide all necessary methods to manipulate
//!   the content.
//! * Minimize the library magic. As many mechanisms as possible should be open to customization.
//!   This includes the protocol receptor implementations but not the core structures of data
//!   reprsentations.
//!
//! Only interpreting to a packet's content by referencing the memory region in which it is
//! contained would require reinterpreting all layers on every mutable access at least.
//! Fortunately, there are two classes of types for which we can trust trait implementations
//! sufficiently well: types local to the crate; and types in the standard library. Thus, the
//! actual representation of parsed packet data needs to be separated from the additional data
//! provided by each layer endpoint (which should be implementable by a user as well). The
//! functionality that provides the packet representation is called `Payload` and `PayloadMut`.
//!
//! ## Things that do not work yet – Future work
//!
//! The same instantiation of a layer can not be simply used at multiple points in the callback
//! tree of handlers. Most layer endpoints mutably borrow their persistent state in their send and
//! receive implementations. There are multiple possible ways of avoiding the problem:
//!
//! * Internal mutability and dynamic borrow checking with `RefCell`. The current layers do not
//! need to lend their state to a specific packet. Instead, each method offered by its packet
//! representation mutates some aspects in a local context. The same would also be possible with a
//! shared reference to a `RefCell<_>` of the internal state by calling [`RefCell::borrow_mut`].
//!   Note that it would be possible but mildly hazardous to store the so created `RefMut` within
//! the packet given to the upper layer. Such a layer is not re-entrant. If the upper layer is a
//! tunnel of sorts that unpacks an encapsulated lower layer packet then the reentry will fail to
//! again borrow from the `RefCell`.
//!   A relevant application of this might be an ip layer used in a wireguard implementation.
//!
//! * Sharding. Split the state into independent fragments, each receiving and sending packets
//! independently. The splitting function can be based on ip subnet or on a hash for example.
//!
//! * Buffer received packets which is definitely the least preferred option.
//!
//! ## Things that do not work – Restrictions
//!
//! ## Previous design choices and Things that need to be thought over
//!
//! TODO: these are somewhat raw thoughts. Expand with justification.
//!
//! How packet buffers are behind a reference, instead of owning a buffer like in some other
//! zerocopy network stacks. There are some points to make either way but Rust's type system gives
//! the former choice a sane interface even when the original pointer is placed within structures.
//! It also works better with the trait design. You can still own the packet buffer but it requires
//! an explicit method on behalf of the specific nic.
//!
//! Receiving not taking an acknowledgement, dropping packets afterwards if they are not used to
//! answer. This also drops some packet buffers where filtered packets could be deinitialized and
//! reused as raw packets immediately.
//!
//! The limited vectorization support and duplicate device handles in nic batched rx/tx. Especially
//! for sending there are overheads in route lookup etc. that could be avoided by batching packet.
//! Might also save on capability information and timestamp queries.

pub mod arp;
pub mod eth;
pub mod icmp;
pub mod ip;
pub mod loss;
pub mod udp;
pub mod tcp;

/// A shortened result type for a generic layer operation.
pub type Result<T> = core::result::Result<T, Error>;

/// An error type for layer operations.
///
/// These variants explicitely do not capture error cases that happen on the network or as part of
/// the logical protocol interactions but express adverse conditions that are caused by some
/// property or configuration within a layer used during the send or receive process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// The operation was not permitted.
    ///
    /// Returned when the device, endpoint, receiver or sender does not allow or implement an
    /// operation. Might also be returned when requesting behaviour that contradicts a standard.
    /// These should (and will mostly) have a configurable flag to turn the error off, though.
    Illegal,

    /// Not enough space for the requested packet.
    ///
    /// May also be returned when trying to resize a packet but the requested length can not be
    /// fulfilled. In contrast to `Illegal` this would signal that a smaller size is possible.
    BadSize,

    /// Unable to find a route towards the destination address.
    Unreachable,

    /// The action could not be completed because there were not enough resources.
    ///
    /// The main difference towards `Illegal` is that implies that it would have been legal with
    /// more resources. If you get this return value you may want to perform manual cleanup if
    /// possible or gargabe collect.
    Exhausted,
    // TODO
}

/// A standard wrapper for a function implementing receive or send traits.
///
/// Keeps the type alias overhead low by providing a single wrapper type that implements the send
/// and receive traits for all layers, where applicable.
pub struct FnHandler<F>(pub F);

/// Can convert from a wire error.
///
/// This indicates some layer tried to operate on a packet but failed.
impl From<crate::wire::Error> for Error {
    fn from(_: crate::wire::Error) -> Self {
        Error::Illegal
    }
}

/// Can convert from a payload error.
///
/// One common cause is failure to resize the buffer to the necessary size.
impl From<crate::wire::PayloadError> for Error {
    fn from(err: crate::wire::PayloadError) -> Self {
        use crate::wire::PayloadError;
        match err {
            PayloadError::BadSize => Error::BadSize,
        }
    }
}
