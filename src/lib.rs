//! Implements the [packet-stream protocol](https://github.com/ssbc/packet-stream) in rust.
#![deny(missing_docs)]

extern crate atm_async_utils;
#[macro_use]
extern crate futures_core;
extern crate futures_sink;
extern crate futures_io;
extern crate futures_util;
extern crate multi_producer_sink;
extern crate multi_consumer_stream;
extern crate packet_stream_codec;

#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate futures;

use std::cell::RefCell;
use std::i32::MAX;
use std::rc::Rc;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use atm_async_utils::SendClose;
use futures_core::{Future, Stream, Poll};
use futures_core::Async::{Ready, Pending};
use futures_core::task::Context;
use futures_io::{AsyncRead, AsyncWrite, Error as IoError};
use futures_sink::Sink;
use multi_producer_sink::{mps, MPS, Done as MPSDone};
use multi_consumer_stream::*;
use packet_stream_codec::{PacketId, CodecSink, CodecStream, TYPE_BINARY, TYPE_STRING, TYPE_JSON,
                          END, STREAM, Metadata as CodecMetadata};

/// An enumeration representing the different types a packet can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    /// Raw binary data.
    Binary,
    /// A utf-8 encoded string.
    String,
    /// A valid piece of json.
    Json,
}

impl PacketType {
    fn flags(&self) -> u8 {
        match *self {
            PacketType::Binary => TYPE_BINARY,
            PacketType::String => TYPE_STRING,
            PacketType::Json => TYPE_JSON,
        }
    }
}

/// The metadata associated with a packet.
///
/// The packet id is an implementation detail of packet-stream and is thus not
/// exposed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The type of the packet.
    pub packet_type: PacketType,
    /// Whether the packet has the end/error flag set.
    pub is_end: bool,
}

impl Metadata {
    /// Create new `Metadata`.
    pub fn new(packet_type: PacketType, is_end: bool) -> Metadata {
        Metadata {
            packet_type,
            is_end,
        }
    }

    fn flags(&self) -> u8 {
        if self.is_end {
            self.packet_type.flags() | END
        } else {
            self.packet_type.flags()
        }
    }

    fn from_decoded_metadata(metadata: CodecMetadata) -> Metadata {
        let packet_type = if metadata.is_buffer_packet() {
            PacketType::Binary
        } else if metadata.is_string_packet() {
            PacketType::String
        } else if metadata.is_json_packet() {
            PacketType::Json
        } else {
            unreachable!()
        };

        Metadata {
            packet_type,
            is_end: metadata.is_end_packet(),
        }
    }

    fn to_codec_metadata(self, id: PacketId) -> CodecMetadata {
        CodecMetadata {
            flags: self.flags(),
            id,
        }
    }
}

/// A future that emits the wrapped writer of a packet-stream once the outgoing half of the stream
/// has been fully closed, it has errored, or once all references to it have been dropped.
pub struct Done<W, B>(MPSDone<CodecSink<W, B>>);

impl<W, B> Future for Done<W, B> {
    type Item = W;
    type Error = W;

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        match self.0.poll(cx) {
            Ok(Ready(sink)) => Ok(Ready(sink.into_inner())),
            Ok(Pending) => Ok(Pending),
            Err(sink) => Err(sink.into_inner()),
        }
    }
}

/// Take ownership of an AsyncRead and an AsyncWrite to create the two halves of a packet-stream,
/// as well as a future for notification when the read-half of the packet-stream has been closed,
/// errored or is not referenced anymore.
///
/// `R` is the `AsyncRead` for reading bytes from the peer, `W` is the
/// `AsyncWrite` for writing bytes to the peer, and `B` is the type that is used
/// as input for sending data.
pub fn packet_stream<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>>
    (r: R,
     w: W)
     -> (PsIn<R, W, B>, PsOut<R, W, B>, Done<W, B>) {
    let (shared, closed) = Shared::new(r, w);
    let shared = Rc::new(RefCell::new(shared));

    (PsIn(Rc::clone(&shared)), PsOut(shared), closed)
}

/// Shared state between a PsIn, PsOut and requests, duplexes, etc.
struct Shared<R, W, B>
    where R: AsyncRead
{
    sink: MPS<CodecSink<W, B>>,
    stream: MCS<CodecStream<R>,
                PacketId,
                fn(&(Box<[u8]>, CodecMetadata)) -> PacketId,
                fn(&IoError) -> PacketId>,
    // The id used for the next actively sent packet.
    id_counter: PacketId,
    // The next id to be accepted as an active packet from the peer.
    accepted_id: PacketId,
}

fn get_id(item: &(Box<[u8]>, CodecMetadata)) -> PacketId {
    item.1.id
}

fn const_zero(_: &IoError) -> PacketId {
    0
}

impl<R, W, B> Shared<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    fn new(r: R, w: W) -> (Shared<R, W, B>, Done<W, B>) {
        let (sink, done) = mps(CodecSink::new(w));
        (Shared {
             sink,
             stream: MCS::new(CodecStream::new(r), get_id, const_zero),
             id_counter: 1,
             accepted_id: 1,
         },
         Done(done))
    }

    fn next_id(&mut self) -> PacketId {
        let ret = self.id_counter;
        self.increment_id();
        return ret;
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

    fn poll_next(&mut self,
                 cx: &mut Context)
                 -> Poll<Option<(Box<[u8]>, Metadata, IncomingPacket<R, W, B>)>, IoError> {
        match try_ready!(self.stream.poll_next(cx)) {
            Some((data, metadata)) => {
                if metadata.id == self.accepted_id {
                    self.increment_accepted();
                    if metadata.is_stream_packet() {
                        let sink_id = metadata.id * -1;
                        let stream_id = metadata.id;
                        Ok(Ready(Some((data, Metadata::from_decoded_metadata(metadata), IncomingPacket::Duplex(PsSink::new(self.sink.clone(), sink_id),
                        PsStream::new(self.stream.mcs_handle(stream_id)))))))
                    } else {
                        Ok(Ready(Some((data,
                                       Metadata::from_decoded_metadata(metadata),
                                       IncomingPacket::Request(PeerRequest::new(self.sink
                                                                                  .clone(),
                                                                              metadata.id))))))
                    }
                } else {
                    // Packet is neither an incoming request nor does it open
                    // a new stream, so ignore it
                    self.poll_next(cx)
                }
            }
            None => Ok(Ready(None)),
        }
    }
}

/// A stream of incoming requests from the peer.
pub struct PsIn<R: AsyncRead, W, B>(Rc<RefCell<Shared<R, W, B>>>);

impl<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> Stream for PsIn<R, W, B> {
    /// The payload of the packet that triggered this emission, the metadata of
    /// the packet, and a way of interacting with the peer.
    type Item = (Box<[u8]>, Metadata, IncomingPacket<R, W, B>);
    type Error = IoError;

    fn poll_next(&mut self, cx: &mut Context) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.borrow_mut().poll_next(cx)
    }
}

/// Allows sending packets to the peer.
pub struct PsOut<R: AsyncRead, W, B>(Rc<RefCell<Shared<R, W, B>>>);

impl<R, W, B> PsOut<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    /// Obtain a future for sending a request to the peer.
    pub fn request(&self, data: B, t: PacketType) -> (Request<W, B>, Response<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (Request::new(ps.sink.clone(), data, t, id), Response::new(ps.stream.mcs_handle(id * -1)))
    }

    /// Create a bidirectional channel multiplexed over the underlying
    /// `AsyncRead`/`AsyncWrite` pair.
    pub fn duplex(&self) -> (PsSink<W, B>, PsStream<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (PsSink::new(ps.sink.clone(), id), PsStream::new(ps.stream.mcs_handle(id * -1)))
    }

    /// Close the packet-stream, indicating that no more packets will be sent.
    ///
    /// This does not immediately close if there are still unfinished
    /// `Request`s, `PeerResponse`s or `PsSink`s. In that case, the closing
    /// happens when the last of them finishes.
    ///
    /// The error contains a `None` if an `Request`, `PeerResponse` or
    /// `PsSink` errored previously.
    pub fn close(self) -> ClosePs<R, W, B> {
        ClosePs(self.0)
    }
}

/// Future for closing the packet-stream, indicating that no more packets will be sent.
///
/// This does not immediately close if there are still unfinished
/// `Request`s, `PeerResponse`s or `PsSink`s. In that case, the closing
/// happens when the last of them finishes.
///
/// The error contains a `None` if an `Request`, `PeerResponse` or
/// `PsSink` errored previously.
pub struct ClosePs<R: AsyncRead, W, B>(Rc<RefCell<Shared<R, W, B>>>);

impl<R, W, B> Future for ClosePs<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    type Item = ();
    type Error = Option<IoError>;

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        self.0.borrow_mut().sink.poll_close(cx)
    }
}

/// An incoming packet, initiated by the peer.
///
/// The enum variants carry values that allow interacting with the peer.
pub enum IncomingPacket<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> {
    /// An incoming request. You get an `PeerRequest`, the peer got a
    /// `Request` and an `Response`.
    Request(PeerRequest<W, B>),
    /// A duplex connection initiated by the peer. Both peers get a `PsSink` and
    /// a `PsStream`.
    Duplex(PsSink<W, B>, PsStream<R>),
}

/// The sink half of a duplex multiplexed over the packet-stream.
pub struct PsSink<W: AsyncWrite, B: AsRef<[u8]>> {
    sink: MPS<CodecSink<W, B>>,
    id: PacketId,
}

impl<W, B> PsSink<W, B>
    where W: AsyncWrite,
          B: AsRef<[u8]>
{
    fn new(sink: MPS<CodecSink<W, B>>, id: PacketId) -> PsSink<W, B> {
        PsSink { sink, id }
    }
}

impl<W, B> Sink for PsSink<W, B>
    where W: AsyncWrite,
          B: AsRef<[u8]>
{
    type SinkItem = (B, Metadata);
    /// The error contains a `None` if an `Request`, `PeerResponse` or
    /// `PsSink` errored previously.
    type SinkError = Option<IoError>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<(), Self::SinkError> {
        self.sink.poll_ready(cx)
    }

    fn start_send(&mut self, item: Self::SinkItem) -> Result<(), Self::SinkError> {
        let flags = item.1.flags() | STREAM;

        self.sink
            .start_send((item.0, CodecMetadata { flags, id: self.id }))
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<(), Self::SinkError> {
        self.sink.poll_flush(cx)
    }

    fn poll_close(&mut self, cx: &mut Context) -> Poll<(), Self::SinkError> {
        self.sink.poll_close(cx)
    }
}

/// An error indicating what happend on a connection.
#[derive(Debug)]
pub enum ConnectionError {
    /// The underlying connection has been closed.
    ClosedEarly,
    /// An error occured on reading.
    Errored(IoError),
}

impl Display for ConnectionError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        match *self {
            ConnectionError::ClosedEarly => write!(f, "ConnectionError::Closed"),
            ConnectionError::Errored(ref err) => write!(f, "ConnectionError::Errored: {}", err),
        }
    }
}

impl Error for ConnectionError {
    fn description(&self) -> &str {
        match *self {
            ConnectionError::ClosedEarly => "Peer closed its write-half of the packet-stream",
            ConnectionError::Errored(ref err) => err.description(),
        }
    }
}

impl From<futures_io::Error> for ConnectionError {
    fn from(err: IoError) -> ConnectionError {
        ConnectionError::Errored(err)
    }
}

type StreamHandle<R> = MCSHandle<CodecStream<R>,
                                 PacketId,
                                 fn(&(Box<[u8]>, CodecMetadata)) -> PacketId,
                                 fn(&IoError) -> PacketId>;

/// The stream half of a duplex multiplexed over the packet-stream.
pub struct PsStream<R: AsyncRead> {
    stream: StreamHandle<R>,
}

impl<R: AsyncRead> PsStream<R> {
    fn new(stream: StreamHandle<R>) -> PsStream<R> {
        PsStream { stream }
    }
}

/// Note that the stream never emits `Ok(None)`.
impl<R: AsyncRead> Stream for PsStream<R> {
    type Item = (Box<[u8]>, Metadata);
    /// If the peer closes the packet-stream, this emits a
    /// `ConnectionError::Closed`. If an error happens/happened on the underlying
    /// `AsyncRead`, this emits a `ConnectionError::Errored`.
    type Error = ConnectionError;

    /// Note that the stream never emits `Ok(None)`.
    fn poll_next(&mut self, cx: &mut Context) -> Poll<Option<Self::Item>, Self::Error> {
        match self.stream.poll_next(cx) {
            Ok(Ready(Some((data, codec_metadata)))) => {
                Ok(Ready((Some((data, Metadata::from_decoded_metadata(codec_metadata))))))
            }
            Ok(Ready(None)) => Err(ConnectionError::ClosedEarly),
            Ok(Pending) => Ok(Pending),
            Err(err) => Err(err.into()),
        }
    }
}

/// A request initated by the peer. Drop to ignore it, or use `respond` to send
/// a response.
pub struct PeerRequest<W, B> {
    sink: MPS<CodecSink<W, B>>,
    id: PacketId,
}

impl<W, B> PeerRequest<W, B> {
    fn new(sink: MPS<CodecSink<W, B>>, id: PacketId) -> PeerRequest<W, B> {
        PeerRequest { sink, id }
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> PeerRequest<W, B> {
    /// Returns a Future which completes once the given packet has been sent to
    /// the peer.
    pub fn respond(self, data: B, metadata: Metadata) -> PeerResponse<W, B> {
        PeerResponse::new(self.sink, self.id * -1, data, metadata)
    }
}

/// An outgoing request, initated by this packet-stream.
///
/// Poll it to actually start sending the request.
pub struct Request<W: AsyncWrite, B: AsRef<[u8]>>(SendClose<MPS<CodecSink<W, B>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> Request<W, B> {
    fn new(sink_handle: MPS<CodecSink<W, B>>,
           data: B,
           t: PacketType,
           id: PacketId)
           -> Request<W, B> {
        Request(SendClose::new(sink_handle,
                               (data,
                                packet_stream_codec::Metadata {
                                    flags: t.flags(),
                                    id,
                                })))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for Request<W, B> {
    type Item = ();
    /// The error contains a `None` if an `Request`, `PeerResponse` or
    /// `PsSink` errored previously.
    type Error = Option<IoError>;

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        let _ = try_ready!(self.0.poll(cx));
        Ok(Ready(()))
    }
}

/// A response that will be received from the peer.
pub struct Response<R: AsyncRead>(StreamHandle<R>);

impl<R: AsyncRead> Response<R> {
    fn new(stream: StreamHandle<R>) -> Response<R> {
        Response(stream)
    }
}

impl<R: AsyncRead> Future for Response<R> {
    type Item = (Box<[u8]>, Metadata);
    /// If the peer closes the packet-stream, this emits a
    /// `ConnectionError::Closed`. If an error happens/happened on the underlying
    /// `AsyncRead`, this emits a `ConnectionError::Errored`.
    type Error = ConnectionError;

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        match self.0.poll_next(cx) {
            Ok(Ready(Some((data, codec_metadata)))) => {
                Ok(Ready(((data, Metadata::from_decoded_metadata(codec_metadata)))))
            }
            Ok(Ready(None)) => Err(ConnectionError::ClosedEarly),
            Ok(Pending) => Ok(Pending),
            Err(err) => Err(err.into()),
        }
    }
}

/// Future that completes when the response has been sent to the peer.
pub struct PeerResponse<W: AsyncWrite, B: AsRef<[u8]>>(SendClose<MPS<CodecSink<W, B>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> PeerResponse<W, B> {
    fn new(sink: MPS<CodecSink<W, B>>,
           id: PacketId,
           data: B,
           metadata: Metadata)
           -> PeerResponse<W, B> {
        debug_assert!(id < 0);

        PeerResponse(SendClose::new(sink, (data, metadata.to_codec_metadata(id))))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for PeerResponse<W, B> {
    type Item = ();
    /// The error contains a `None` if an `Request`, `PeerResponse` or
    /// `PsSink` errored previously.
    type Error = Option<IoError>;

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        let _ = try_ready!(self.0.poll(cx));
        Ok(Ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_ringbuffer::*;
    use futures::prelude::*;
    use futures::future::ok;
    use futures::stream::iter_ok;
    use futures::executor::block_on;

    #[test]
    fn test_requests() {
        let (writer_a, reader_a) = ring_buffer(2);
        let (writer_b, reader_b) = ring_buffer(2);

        let (a_in, a_out, _) = packet_stream(reader_a, writer_b);
        let (b_in, b_out, _) = packet_stream(reader_b, writer_a);

        let echo = b_in.for_each(|(data, metadata, in_packet)| match in_packet {
                                     IncomingPacket::Request(in_request) => {
                                         in_request
                                             .respond(data, metadata)
                                             .map_err(|_| unreachable!())
                                     }
                                     IncomingPacket::Duplex(_, _) => unreachable!(),
                                 })
            .and_then(|_| b_out.close().map_err(|_| unreachable!()));

        let consume_a = a_in.for_each(|_| ok(()));

        let (req0, res0) = a_out.request([0], PacketType::Binary);
        let (req1, res1) = a_out.request([1], PacketType::Binary);
        let (req2, res2) = a_out.request([2], PacketType::Binary);

        let send_all = req0.join3(req1, req2).and_then(|_| a_out.close());

        let receive_all = res0.join3(res1, res2)
            .map(|((r0_data, r0_meta), (r1_data, r1_meta), (r2_data, r2_meta))| {
                     return r0_data == vec![0u8].into_boxed_slice() &&
                            r0_meta.packet_type == PacketType::Binary &&
                            r1_data == vec![1u8].into_boxed_slice() &&
                            r1_meta.packet_type == PacketType::Binary &&
                            r2_data == vec![2u8].into_boxed_slice() &&
                            r2_meta.packet_type == PacketType::Binary;
                 });

        assert!(block_on(echo.join4(consume_a.map_err(|_| unreachable!()),
                                    send_all.map_err(|_| unreachable!()),
                                    receive_all.map_err(|_| unreachable!()))
                             .map(|(_, _, _, worked)| assert!(worked)))
                        .is_ok());
    }

    #[test]
    fn test_duplexes() {
        let (writer_a, reader_a) = ring_buffer(2);
        let (writer_b, reader_b) = ring_buffer(2);

        let (a_in, a_out, _) = packet_stream(reader_a, writer_b);
        let (b_in, b_out, _) = packet_stream(reader_b, writer_a);

        let non_end = Metadata::new(PacketType::Binary, false);
        let end = Metadata::new(PacketType::Binary, true);

        let echo = b_in.map_err(|_| unreachable!())
            .for_each(|(_, _, in_packet)| match in_packet {
                          IncomingPacket::Request(_) => unreachable!(),
                          IncomingPacket::Duplex(sink, stream) => {
                              stream
                                  .map_err::<Option<IoError>, _>(|_| None)
                                  .take_while(|&(_, metadata)| ok(!metadata.is_end))
                                  .map(|(data, _)| (data, non_end))
                                  .forward(sink)
                                  .map(|_| ())
                          }
                      })
            .and_then(|_| b_out.close().map_err(|_| unreachable!()));

        let consume_a = a_in.for_each(|_| ok(()));

        let (sink0_a, stream0_a) = a_out.duplex();
        let (sink1_a, stream1_a) = a_out.duplex();
        let (sink2_a, stream2_a) = a_out.duplex();

        let send_0 = sink0_a.send_all(iter_ok::<_, IoError>(vec![(vec![0], non_end),
                                                                 (vec![0], non_end),
                                                                 (vec![42], end)]));
        let send_1 = sink1_a.send_all(iter_ok::<_, IoError>(vec![(vec![1], non_end),
                                                                 (vec![1], non_end),
                                                                 (vec![1], non_end),
                                                                 (vec![43], end)]));
        let send_2 = sink2_a.send_all(iter_ok::<_, IoError>(vec![(vec![2], non_end),
                                                                 (vec![2], non_end),
                                                                 (vec![2], non_end),
                                                                 (vec![2], non_end),
                                                                 (vec![44], end)]));
        let send_all = send_0.join3(send_1, send_2).and_then(|_| a_out.close());

        let receive_0 = stream0_a
            .take(1)
            .fold(false, |_, (data, _)| ok(data == vec![0].into_boxed_slice()));
        let receive_1 = stream1_a
            .take(2)
            .fold(true,
                  |acc, (data, _)| ok(acc && data == vec![1].into_boxed_slice()));
        let receive_2 = stream2_a
            .take(3)
            .fold(true,
                  |acc, (data, _)| ok(acc && data == vec![2].into_boxed_slice()));
        let receive_all = receive_0
            .join3(receive_1, receive_2)
            .map(|(a, b, c)| a && b && c);

        assert!(block_on(echo.join4(consume_a.map_err(|_| unreachable!()),
                                    send_all.map_err(|_| unreachable!()),
                                    receive_all.map_err(|_| unreachable!()))
                             .map(|(_, _, _, worked)| assert!(worked)))
                        .is_ok());
    }
}
