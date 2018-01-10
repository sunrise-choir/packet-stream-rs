use std::marker::PhantomData;
use std::io;
use std::cell::RefCell;

use futures::{Stream, Poll, Future, Sink, AsyncSink, StartSend};
use tokio_io::{AsyncRead, AsyncWrite};
use void::Void;
use bytes::Buf;
use multi_producer_sink::{OwnerMPS, Handle};
use multi_consumer_stream::{OwnerMCS, DefaultHandle, KeyHandle};

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

// /// Wrapper around an AsyncRead and an AsyncWrite to allow running a
// /// packet-stream over them.
// pub struct PsOwner<R, W>
//     where R: AsyncRead,
//           W: AsyncWrite
// {
//     sink: OwnerMPS<TODO<W>>,
//     stream: OwnerMCS<TODO2<R>, PacketId, i8>,
//     state: RefCell<Shared>,
// }
//
// impl<R: AsyncRead, W: AsyncWrite> PsOwner<R, W> {
//     /// Take ownership of an AsyncRead and an AsyncWrite to create a `PsOwner`.
//     pub fn new(r: R, w: W) -> PsOwner<R, W> {
//         PsOwner {
//             sink: OwnerMPS::new(sink),
//             stream: OwnerMCS::new(stream),
//             state: RefCell::new(Shared::new()),
//         }
//     }
// }
//
// // State for the PsOwner.
// struct Shared {
//     id_counter: PacketId,
// }
//
// impl Shared {
//     fn new() -> Shared {
//         Shared { id_counter: 1 }
//     }
// }

// /// Creates a packet stream over an asynchronous Read and Write.
// /// The packet stream consists of two halves: `PsIn` for receiving data, and
// /// `PsOut` for sending data.
// ///
// /// `B` is the type of `Buf` used to write to the peer via streams.
// pub fn create_ps<R: AsyncRead, W: AsyncWrite, B: Buf>(read: R,
//                                                       write: W)
//                                                       -> (PsIn<R, B>, PsOut<W, B>) {
//     unimplemented!()
// }
//
// /// The input half of a packet stream. This implements the `Stream` trait. The
// /// stream must be consumed, even if packets initiated by the peer are ignored.
// /// If the stream is not consumed, incoming responses and substream data can not
// /// be handled correctly.
// pub struct PsIn<R, B> {
//     _read: PhantomData<R>,
//     _buf: PhantomData<B>,
// }
//
// impl<R: AsyncRead, B: Buf> Stream for PsIn<R, B> {
//     type Item = IncomingPacket<B>;
//     type Error = PsInError;
//
//     fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
//         unimplemented!()
//     }
// }
//
// /// An error in the receiving half of a packet stream.
// pub enum PsInError {
//     /// An error on the underlying transport.
//     IoError(io::Error),
//     /// The underlying stream ended although the peer did not send a goodbye packet.
//     UnexpectedEndOfStream,
// }
//
// /// An incoming packet, initiated by the peer.
// pub enum IncomingPacket<B: Buf> {
//     /// An incoming request.
//     Request(InRequest),
//     /// A duplex connection initiated by the peer.
//     Duplex(PsDuplex<B>),
// }
//
// /// A request initated by the peer. Drop to ignore it, or use `respond` to send
// /// a response.
// pub struct InRequest {}
//
// impl InRequest {
//     /// Returns a Future which completes once the given packet has been sent to
//     /// the peer. Uses a type-level parameter `P` to control which flags to set.
//     fn respond<B: AsRef<[u8]>, P: PacketType>(bytes: B) -> SendResponse<B, P> {
//         unimplemented!()
//     }
// }
//
// impl Drop for InRequest {
//     fn drop(&mut self) {
//         unimplemented!()
//     }
// }
//
// /// Future that completes whan the given bytes have been sent as a response to
// /// the peer.
// pub struct SendResponse<B, P> {
//     _bytes: PhantomData<B>,
//     _packet: PhantomData<P>,
// }
//
// impl<B: AsRef<[u8]>, P: PacketType> Future for SendResponse<B, P> {
//     type Item = ();
//     type Error = SendError<B>;
//
//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         unimplemented!()
//     }
// }
//
// /// An error during sending some bytes over a packet stream.
// pub enum SendError<B> {
//     /// An error on the underlying transport.
//     IoError(io::Error, B),
//     /// The packet stream has already been closed (or closed during a send attempt).
//     ClosedPs(B),
// }
//
// /// An enumeration representing the different types a packet can have.
// pub enum PsPacketType {
//     /// Raw binary data.
//     Binary,
//     /// A utf-8 encoded string.
//     String,
//     /// A valid piece of json.
//     Json,
// }
//
// /// A duplex connection multiplexed over the packet stream.
// // TODO dropping?
// pub struct PsDuplex<B: Buf> {
//     _buf: PhantomData<B>,
// }
//
// /// The Stream implementation of a PsDuplex is used to consume values from the
// /// peer. All connection-level errors are emitted on the main `PsIn`, so the
// /// `PsDuplex` Stream implementation never errors.
// impl<B: Buf> Stream for PsDuplex<B> {
//     type Item = DecodedPacket;
//     type Error = Void;
//
//     fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
//         unimplemented!()
//     }
// }
//
// impl<B: Buf> PsDuplex<B> {
//     /// Start sending a packet of raw bytes. This is like the `start_send`
//     /// method of the Sink impl, but without runtime overhead for the type.
//     fn start_send_binary(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
//         unimplemented!()
//     }
//
//     /// Start sending a string packet. This is like the `start_send`
//     /// method of the Sink impl, but without runtime overhead for the type.
//     fn start_send_string(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
//         unimplemented!()
//     }
//
//     /// Start sending a json packet. This is like the `start_send`
//     /// method of the Sink impl, but without runtime overhead for the type.
//     fn start_send_json(&mut self, item: B) -> StartSend<B, Option<io::Error>> {
//         unimplemented!()
//     }
// }
//
// /// The Sink implementation of a PsDuplex is used to send values to the peer.
// /// This implementation sets the packet type of outgoing packets explicitly via
// /// PsPacketTypes. It can also be converted into a sink which always sends the
// /// same packet type (which is slightly more efficient).
// impl<B: Buf> Sink for PsDuplex<B> {
//     type SinkItem = (B, PsPacketType);
//     type SinkError = PsSendError;
//
//     fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
//         match item.1 {
//             PsPacketType::Binary => {
//                 self.start_send_binary(item.0)
//                     .map(|a_s| match a_s {
//                              AsyncSink::Ready => AsyncSink::Ready,
//                              AsyncSink::NotReady(it) => {
//                                  AsyncSink::NotReady((it, PsPacketType::Binary))
//                              }
//                          })
//             }
//
//             PsPacketType::String => {
//                 self.start_send_string(item.0)
//                     .map(|a_s| match a_s {
//                              AsyncSink::Ready => AsyncSink::Ready,
//                              AsyncSink::NotReady(it) => {
//                                  AsyncSink::NotReady((it, PsPacketType::String))
//                              }
//                          })
//             }
//
//             PsPacketType::Json => {
//                 self.start_send_json(item.0)
//                     .map(|a_s| match a_s {
//                              AsyncSink::Ready => AsyncSink::Ready,
//                              AsyncSink::NotReady(it) => {
//                                  AsyncSink::NotReady((it, PsPacketType::Json))
//                              }
//                          })
//             }
//         }
//     }
//
//     fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
//         unimplemented!()
//     }
//
//     fn close(&mut self) -> Poll<(), Self::SinkError> {
//         unimplemented!()
//     }
// }
//
// // TODO PsOut: Implements clone so that multiple of the methods which need mutable references can be used without problems.
