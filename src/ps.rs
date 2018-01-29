use std::marker::PhantomData;
use std::io;
use std::cell::RefCell;
use std::i32::MAX;

use futures::{Stream, Poll, Future, Sink, AsyncSink, StartSend, Async, AndThen};
use futures::sink::Send;
use futures::stream::StreamFuture;
use futures::future::{Map, MapErr};
use tokio_io::{AsyncRead, AsyncWrite};
use void::Void;
use multi_producer_sink::{OwnerMPS, Handle};
use multi_consumer_stream::{OwnerMCS, DefaultHandle, KeyHandle};
use atm_async_utils::sink_futures::Close;

use codec::*;

/// An enumeration representing the different types a packet can have.
pub enum PsPacketType {
    /// Raw binary data.
    Binary,
    /// A utf-8 encoded string.
    String,
    /// A valid piece of json.
    Json,
}

impl PsPacketType {
    fn flags(&self) -> u8 {
        match *self {
            PsPacketType::Binary => TYPE_BINARY,
            PsPacketType::String => TYPE_STRING,
            PsPacketType::Json => TYPE_JSON,
        }
    }
}

/// Wrapper around an AsyncRead and an AsyncWrite to allow running a
/// packet-stream over them.
///
/// `R` is the `AsyncRead` for reading bytes from the peer, `W` is the
/// `AsyncWrite` for writing bytes to the peer, and `B` is the type that is used
/// as input for sending data.
pub struct PsOwner<R, W, B: AsRef<[u8]>>
    where R: AsyncRead,
          W: AsyncWrite
{
    sink: OwnerMPS<PSCodecSink<W, B>>,
    stream: OwnerMCS<PSCodecStream<R>, PacketId, fn(&DecodedPacket) -> PacketId>,
    state: RefCell<Shared>,
}

impl<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> PsOwner<R, W, B> {
    /// Take ownership of an AsyncRead and an AsyncWrite to create a `PsOwner`.
    pub fn new(r: R, w: W) -> PsOwner<R, W, B> {
        PsOwner {
            sink: OwnerMPS::new(PSCodecSink::new(w)),
            stream: OwnerMCS::new(PSCodecStream::new(r), DecodedPacket::id),
            state: RefCell::new(Shared::new()),
        }
    }

    /// Obtain a `PsIncoming`, used to receive packets from the peer.
    ///
    /// When using packet-stream, you must always create exactly one `PsIncoming`,
    /// and it must be consumed. Otherwise, the whole incoming traffic is blocked
    /// once the peer send a packet that is neither a response nor part of a
    /// duplex stream.
    ///
    /// Yes, the *exactly one* requirement is ugly, but the upside of this API
    /// is that we can use lifetimes to make sure that nothing that uses the
    /// packet-stream (i.e. substreams and requests) outlives it.
    pub fn incoming(&self) -> PsIncoming<R, W, B> {
        PsIncoming::new(self)
    }

    /// Send a request to the peer.
    ///
    /// The first returned Future must be polled to actually start sending the
    /// request. The second Future can be polled to receive the response.
    pub fn request(&self, data: B, t: PsPacketType) -> (SendRequest<W, B>, InResponse<R>) {
        let id = self.next_id();
        (SendRequest::new(self.sink.handle(), data, t, id),
         InResponse::new(self.stream.key_handle(id * -1)))
    }

    // TODO create duplexes


    fn next_id(&self) -> PacketId {
        let mut state = self.state.borrow_mut();
        let ret = state.id_counter;
        state.increment_id();
        return ret;
    }
}

// State for the PsOwner.
struct Shared {
    // The id used for the next actively sent packet.
    id_counter: PacketId,
    // The next id to be accepted as an active packet from the peer.
    accepted_id: PacketId,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            id_counter: 1,
            accepted_id: 1,
        }
    }

    fn increment_id(&mut self) {
        if self.id_counter == MAX {
            self.id_counter = 1;
        } else {
            self.id_counter += 1
        }
    }

    fn increment_accepted(&mut self) {
        if self.accepted_id == MAX {
            self.accepted_id = 1;
        } else {
            self.accepted_id += 1
        }
    }
}

/// A request initated by this packet-stream.
///
/// Poll it to actually start sending the request.
pub struct SendRequest<'owner, W: 'owner + AsyncWrite, B: 'owner + AsRef<[u8]>>(
        AndThen<
            Send<Handle<'owner, PSCodecSink<W, B>>>,
            Close<Handle<'owner, PSCodecSink<W, B>>>,
            fn(Handle<'owner, PSCodecSink<W, B>>) -> Close<Handle<'owner, PSCodecSink<W, B>>>
        >);

impl<'owner, W: AsyncWrite, B: AsRef<[u8]>> SendRequest<'owner, W, B> {
    fn new(sink_handle: Handle<'owner, PSCodecSink<W, B>>,
           data: B,
           t: PsPacketType,
           id: PacketId)
           -> SendRequest<'owner, W, B> {
        SendRequest(sink_handle
                        .send((data,
                               MetaData {
                                   flags: t.flags(),
                                   id,
                               }))
                        .and_then(|s| Close::new(s)))
    }
}

/// A response that will be received from the peer.
pub struct InResponse<'owner, R: 'owner + AsyncRead>(KeyHandle<'owner,
                                                                PSCodecStream<R>,
                                                                PacketId,
                                                                fn(&DecodedPacket) -> PacketId>);

impl<'owner, R: AsyncRead> InResponse<'owner, R> {
    fn new(stream_handle: KeyHandle<'owner,
                                    PSCodecStream<R>,
                                    PacketId,
                                    fn(&DecodedPacket) -> PacketId>)
           -> InResponse<'owner, R> {
        InResponse(stream_handle)
    }
}

impl<'owner, R: AsyncRead> Future for InResponse<'owner, R> {
    type Item = DecodedPacket;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.poll() {
            Ok(Async::Ready(Some(decoded_packet))) => Ok(Async::Ready(decoded_packet)),
            Ok(Async::Ready(None)) => {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof,
                                   "packet-stream closed before response was received"))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err(e),                      
        }
    }
}

pub struct PsIncoming<'owner, R: 'owner + AsyncRead, W: 'owner + AsyncWrite, B: 'owner> {
    stream_handle:
        DefaultHandle<'owner, PSCodecStream<R>, PacketId, fn(&DecodedPacket) -> PacketId>,
    sink_handle: Handle<'owner, PSCodecSink<W, B>>,
    shared: &'owner RefCell<Shared>,
}

impl<'owner, R: 'owner + AsyncRead, W: 'owner + AsyncWrite, B: AsRef<[u8]>> PsIncoming<'owner,
                                                                                       R,
                                                                                       W,
                                                                                       B> {
    fn new(owner: &'owner PsOwner<R, W, B>) -> PsIncoming<'owner, R, W, B> {
        PsIncoming {
            stream_handle: owner.stream.default_handle(),
            sink_handle: owner.sink.handle(),
            shared: &owner.state,
        }
    }
}

impl<'owner, R: 'owner + AsyncRead, W: 'owner + AsyncWrite, B> Stream
    for PsIncoming<'owner, R, W, B> {
    type Item = IncomingPacket<'owner, W, B>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut shared = self.shared.borrow_mut();
        match try_ready!(self.stream_handle.poll()) {
            Some(p) => {
                if p.id() == shared.accepted_id {
                    shared.increment_accepted();
                    if p.is_stream_packet() {
                        unimplemented!()
                        // Ok(Async::Ready(Some(IncomingPacket::Duplex(PsDuplex::new(p,
                        //                                             self.stream_handle
                        //                                                 .key_handle(p.id()))))))
                    } else {
                        Ok(Async::Ready(Some(IncomingPacket::Request(InRequest::new(p, self.sink_handle.clone())))))
                    }
                } else {
                    // Packet is neither an incoming request nor does it open
                    // a new stream, so ignore it
                    self.poll()
                }
            }
            None => Ok(Async::Ready(None)),
        }
    }
}

/// An incoming packet, initiated by the peer.
pub enum IncomingPacket<'owner, W: 'owner, B: 'owner> {
    /// An incoming request.
    Request(InRequest<'owner, W, B>),
    /// A duplex connection initiated by the peer.
    Duplex(PsDuplex<B>),
}

/// A request initated by the peer. Drop to ignore it, or use `respond` to send
/// a response.
pub struct InRequest<'owner, W: 'owner, B: 'owner> {
    packet: DecodedPacket,
    sink_handle: Handle<'owner, PSCodecSink<W, B>>,
}

impl<'owner, W, B> InRequest<'owner, W, B> {
    fn new(packet: DecodedPacket,
           sink_handle: Handle<'owner, PSCodecSink<W, B>>)
           -> InRequest<'owner, W, B> {
        InRequest {
            packet,
            sink_handle,
        }
    }

    /// Returns the packet corresponding to this incoming request.
    fn packet(&self) -> &DecodedPacket {
        &self.packet
    }
}

impl<'owner, W: AsyncWrite, B: AsRef<[u8]>> InRequest<'owner, W, B> {
    /// Returns a Future which completes once the given packet has been sent to
    /// the peer.
    fn respond(self, bytes: B, t: PsPacketType) -> SendResponse<'owner, W, B> {
        SendResponse::new(self.sink_handle, self.packet.id() * -1, bytes, t)
    }
}

/// Future that completes when the given bytes have been sent as a response to
/// the peer.
pub struct SendResponse<'owner, W: 'owner + AsyncWrite, B: 'owner + AsRef<[u8]>>(AndThen<Send<Handle<'owner,
                                                                           PSCodecSink<W, B>>>,
                                                               Close<Handle<'owner,
                                                                            PSCodecSink<W, B>>>,
                                                               fn(Handle<'owner, PSCodecSink<W, B>>) -> Close<Handle<'owner,
                                                                            PSCodecSink<W, B>>>>);

impl<'owner, W: AsyncWrite, B: AsRef<[u8]>> SendResponse<'owner, W, B> {
    fn new(sink_handle: Handle<'owner, PSCodecSink<W, B>>,
           id: PacketId,
           data: B,
           t: PsPacketType)
           -> SendResponse<'owner, W, B> {
        debug_assert!(id < 0);

        SendResponse(sink_handle
                         .send((data,
                                MetaData {
                                    flags: t.flags(),
                                    id: id,
                                }))
                         .and_then(|s| Close::new(s)))
    }
}

impl<'owner, W: AsyncWrite, B: AsRef<[u8]>> Future for SendResponse<'owner, W, B> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

/// A duplex connection multiplexed over the packet stream.
// TODO dropping?
pub struct PsDuplex<B> {
    _buf: PhantomData<B>,
}

/// The Stream implementation of a PsDuplex is used to consume values from the
/// peer.
impl<B> Stream for PsDuplex<B> {
    type Item = DecodedPacket;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        unimplemented!()
    }
}

impl<B> PsDuplex<B> {
    /// Start sending a packet of raw bytes. This is like the `start_send`
    /// method of the Sink impl, but without runtime overhead for the type.
    fn start_send_binary(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
        unimplemented!()
    }

    /// Start sending a string packet. This is like the `start_send`
    /// method of the Sink impl, but without runtime overhead for the type.
    fn start_send_string(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
        unimplemented!()
    }

    /// Start sending a json packet. This is like the `start_send`
    /// method of the Sink impl, but without runtime overhead for the type.
    fn start_send_json(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
        unimplemented!()
    }
}

/// The Sink implementation of a PsDuplex is used to send values to the peer.
/// This implementation sets the packet type of outgoing packets explicitly via
/// PsPacketTypes. It can also be converted into a sink which always sends the
/// same packet type (which is slightly more efficient).
impl<B> Sink for PsDuplex<B> {
    type SinkItem = (B, PsPacketType);
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        unimplemented!()
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        unimplemented!()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        unimplemented!()
    }
}

// TODO PsOut: Implements clone so that multiple of the methods which need mutable references can be used without problems.
