//! Implements the [packet-stream protocol](https://github.com/ssbc/packet-stream) in rust.
#![warn(missing_docs)]

#[macro_use(try_ready)]
extern crate futures;
extern crate tokio_io;
extern crate atm_io_utils;
extern crate atm_async_utils;
// extern crate void; TODO check for unneeded extern crates
extern crate multi_producer_sink;
extern crate multi_consumer_stream;
extern crate packet_stream_codec;

#[cfg(test)]
extern crate partial_io;
#[cfg(test)]
extern crate quickcheck;
#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate rand;
#[cfg(test)]
extern crate tokio;

use std::cell::RefCell;
use std::i32::MAX;
use std::io;
use std::rc::Rc;

use atm_async_utils::sink_futures::Close;
use futures::{Future, Sink, Stream, Poll, Async, StartSend, AsyncSink};
use futures::future::AndThen;
use futures::sink::Send;
use futures::unsync::oneshot::Canceled;
use multi_producer_sink::{mps, MPS};
use multi_consumer_stream::*;
use packet_stream_codec::{PacketId, CodecSink, CodecStream, TYPE_BINARY, TYPE_STRING, TYPE_JSON,
                          END, STREAM};
use tokio_io::{AsyncRead, AsyncWrite};

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
    packet_type: PacketType,
    is_end: bool,
}

impl Metadata {
    fn flags(&self) -> u8 {
        if self.is_end {
            self.packet_type.flags() | END
        } else {
            self.packet_type.flags()
        }
    }

    fn from_decoded_metadata(metadata: packet_stream_codec::Metadata) -> Metadata {
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

    fn to_codec_metadata(self, id: PacketId) -> packet_stream_codec::Metadata {
        packet_stream_codec::Metadata {
            flags: self.flags(),
            id,
        }
    }
}

/// A future that emits the wrapped writer of a packet-stream once the outgoing
/// half of the stream has been fully closed.
pub struct Closed<W, B>(multi_producer_sink::Closed<CodecSink<W, B>>);

impl<W, B> Future for Closed<W, B> {
    type Item = W;
    /// This can only be emitted if a previously polled/written `OutRequest`,
    /// `OutResponse`s or `PsSink` is dropped whithout waiting for it to finish.
    type Error = Canceled;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Ok(Async::Ready(try_ready!(self.0.poll()).into_inner()))
    }
}

/// Take ownership of an AsyncRead and an AsyncWrite to create the two halves of
/// a packet-stream, as well as a future for notification when the packet-stream
/// has been fully closed.
///
/// `R` is the `AsyncRead` for reading bytes from the peer, `W` is the
/// `AsyncWrite` for writing bytes to the peer, and `B` is the type that is used
/// as input for sending data.
pub fn packet_stream<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>>
    (r: R,
     w: W)
     -> (PsIn<R, W, B>, PsOut<R, W, B>, Closed<W, B>) {
    let (shared, closed) = Shared::new(r, w);
    let shared = Rc::new(RefCell::new(shared));

    (PsIn(Rc::clone(&shared)), PsOut(shared), closed)
}

/// Shared state between a PsIn, PsOut and requests, duplexes, etc.
struct Shared<R, W, B>
    where R: AsyncRead
{
    sink: MPS<CodecSink<W, B>>,
    stream:
        MCS<CodecStream<R>, PacketId, fn(&(Box<[u8]>, packet_stream_codec::Metadata)) -> PacketId>,
    // The id used for the next actively sent packet.
    id_counter: PacketId,
    // The next id to be accepted as an active packet from the peer.
    accepted_id: PacketId,
}

fn get_id(item: &(Box<[u8]>, packet_stream_codec::Metadata)) -> PacketId {
    item.1.id
}

impl<R, W, B> Shared<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    fn new(r: R, w: W) -> (Shared<R, W, B>, Closed<W, B>) {
        let (sink, closed) = mps(CodecSink::new(w));
        (Shared {
             sink,
             stream: mcs(CodecStream::new(r), get_id),
             id_counter: 1,
             accepted_id: 1,
         },
         Closed(closed))
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

    fn poll(&mut self) -> Poll<Option<(Box<[u8]>, Metadata, IncomingPacket<R, W, B>)>, io::Error> {
        match try_ready!(self.stream.poll()) {
            Some((data, metadata)) => {
                if metadata.id == self.accepted_id {
                    self.increment_accepted();
                    if metadata.is_stream_packet() {
                        let sink_id = metadata.id * -1;
                        let stream_id = metadata.id;
                        Ok(Async::Ready(Some((data, Metadata::from_decoded_metadata(metadata), IncomingPacket::Duplex(PsSink::new(self.sink.clone(), sink_id),
                        PsStream::new(self.stream.key_handle(stream_id)))))))
                    } else {
                        Ok(Async::Ready(Some((data, Metadata::from_decoded_metadata(metadata), IncomingPacket::Request(InRequest::new(self.sink.clone(), metadata.id))))))
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

/// A stream of incoming requests from the peer.
pub struct PsIn<R: AsyncRead, W, B>(Rc<RefCell<Shared<R, W, B>>>);

impl<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> Stream for PsIn<R, W, B> {
    /// The payload of the packet that triggered this emission, the metadata of
    /// the packet, and a way of interacting with the peer.
    type Item = (Box<[u8]>, Metadata, IncomingPacket<R, W, B>);
    /// The first error of any `InResponse` or `PsStream` is emitted here.
    /// Polling after the first error panics.
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.borrow_mut().poll()
    }
}

/// Allows sending packets to the peer.
pub struct PsOut<R: AsyncRead, W, B>(Rc<RefCell<Shared<R, W, B>>>);

impl<R, W, B> PsOut<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    /// Send a request to the peer.
    ///
    /// The `OutRequest` Future must be polled to actually start sending the
    /// request. The `InResponse` Future can be polled to receive the response.
    pub fn request(&self, data: B, t: PacketType) -> (OutRequest<W, B>, InResponse<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (OutRequest::new(ps.sink.clone(), data, t, id),
         InResponse::new(ps.stream.key_handle(id * -1)))
    }

    /// Create a bidirectional channel multiplexed over the underlying
    /// `AsyncRead`/`AsyncWrite` pair.
    pub fn duplex(&self) -> (PsSink<W, B>, PsStream<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (PsSink::new(ps.sink.clone(), id), PsStream::new(ps.stream.key_handle(id * -1)))
    }

    /// Close the packet-stream, indicating that no more packets will be sent.
    ///
    /// This does not immediately close if there are still unfinished
    /// `OutRequest`s, `OutResponse`s or `PsSink`s. In that case, the closing
    /// happens when the last of them finishes.
    ///
    /// The error contains a `None` if an `OutRequest`, `OutResponse` or
    /// `PsSink` errored previously.
    pub fn close(&mut self) -> Poll<(), Option<io::Error>> {
        self.0.borrow_mut().sink.close()
    }
}

/// An incoming packet, initiated by the peer.
///
/// The enum variants carry values that allow interacting with the peer.
pub enum IncomingPacket<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> {
    /// An incoming request. You get an `InRequest`, the peer got an
    /// `OutRequest` and an `InResponse`.
    Request(InRequest<W, B>),
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
    /// The error contains a `None` if an `OutRequest`, `OutResponse` or
    /// `PsSink` errored previously.
    type SinkError = Option<io::Error>;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let mut flags = item.1.flags() | STREAM;

        match self.sink
                  .start_send((item.0, packet_stream_codec::Metadata { flags, id: self.id })) {
            Ok(AsyncSink::NotReady((bytes, _))) => Ok(AsyncSink::NotReady((bytes, item.1))),
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Err(e) => Err(e),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.close()
    }
}

/// An error indicating what happend on a connection.
pub enum ConnectionError {
    /// The peer closed the connection even though this substream was still
    /// waiting for data.
    Closed,
    /// The connection errored.
    Errored,
}

type StreamHandle<R> = KeyMCS<CodecStream<R>,
                              PacketId,
                              fn(&(Box<[u8]>, packet_stream_codec::Metadata)) -> PacketId>;

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
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.stream.poll() {
            Ok(Async::Ready(Some((data, codec_metadata)))) => {
                Ok(Async::Ready((Some((data, Metadata::from_decoded_metadata(codec_metadata))))))
            }
            Ok(Async::Ready(None)) => Err(ConnectionError::Closed),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(()) => Err(ConnectionError::Errored),
        }
    }
}

/// A request initated by the peer. Drop to ignore it, or use `respond` to send
/// a response.
pub struct InRequest<W, B> {
    sink: MPS<CodecSink<W, B>>,
    id: PacketId,
}

impl<W, B> InRequest<W, B> {
    fn new(sink: MPS<CodecSink<W, B>>, id: PacketId) -> InRequest<W, B> {
        InRequest { sink, id }
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> InRequest<W, B> {
    /// Returns a Future which completes once the given packet has been sent to
    /// the peer.
    pub fn respond(self, data: B, metadata: Metadata) -> OutResponse<W, B> {
        OutResponse::new(self.sink, self.id * -1, data, metadata)
    }
}

/// An outgoing request, initated by this packet-stream.
///
/// Poll it to actually start sending the request.
pub struct OutRequest<W: AsyncWrite, B: AsRef<[u8]>>(AndThen<Send<MPS<CodecSink<W, B>>>,
                                                              Close<MPS<CodecSink<W, B>>>,
                                                              fn(MPS<CodecSink<W, B>>)
                                                                 -> Close<MPS<CodecSink<W, B>>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> OutRequest<W, B> {
    fn new(sink_handle: MPS<CodecSink<W, B>>,
           data: B,
           t: PacketType,
           id: PacketId)
           -> OutRequest<W, B> {
        OutRequest(sink_handle
                       .send((data,
                              packet_stream_codec::Metadata {
                                  flags: t.flags(),
                                  id,
                              }))
                       .and_then(|s| Close::new(s)))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for OutRequest<W, B> {
    type Item = ();
    /// The error contains a `None` if an `OutRequest`, `OutResponse` or
    /// `PsSink` errored previously.
    type Error = Option<io::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let _ = try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

/// A response that will be received from the peer.
pub struct InResponse<R: AsyncRead>(StreamHandle<R>);

impl<R: AsyncRead> InResponse<R> {
    fn new(stream: StreamHandle<R>) -> InResponse<R> {
        InResponse(stream)
    }
}

impl<R: AsyncRead> Future for InResponse<R> {
    type Item = (Box<[u8]>, Metadata);
    /// If the peer closes the packet-stream, this emits a
    /// `ConnectionError::Closed`. If an error happens/happened on the underlying
    /// `AsyncRead`, this emits a `ConnectionError::Errored`.
    type Error = ConnectionError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.poll() {
            Ok(Async::Ready(Some((data, codec_metadata)))) => {
                Ok(Async::Ready(((data, Metadata::from_decoded_metadata(codec_metadata)))))
            }
            Ok(Async::Ready(None)) => Err(ConnectionError::Closed),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(()) => Err(ConnectionError::Errored),
        }
    }
}

/// Future that completes when the response has been sent to the peer.
pub struct OutResponse<W: AsyncWrite, B: AsRef<[u8]>>(AndThen<Send<MPS<CodecSink<W, B>>>,
                                                               Close<MPS<CodecSink<W, B>>>,
                                                               fn(MPS<CodecSink<W, B>>)
                                                                  -> Close<MPS<CodecSink<W, B>>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> OutResponse<W, B> {
    fn new(sink: MPS<CodecSink<W, B>>,
           id: PacketId,
           data: B,
           metadata: Metadata)
           -> OutResponse<W, B> {
        debug_assert!(id < 0);

        OutResponse(sink.send((data, metadata.to_codec_metadata(id)))
                        .and_then(|s| Close::new(s)))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for OutResponse<W, B> {
    type Item = ();
    /// The error contains a `None` if an `OutRequest`, `OutResponse` or
    /// `PsSink` errored previously.
    type Error = Option<io::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let _ = try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//
//     use partial_io::{PartialAsyncRead, PartialAsyncWrite, PartialWithErrors};
//     use partial_io::quickcheck_types::GenInterruptedWouldBlock;
//     use quickcheck::{QuickCheck, StdGen};
//     use async_ringbuffer::*;
//     use rand;
//     use futures::stream::iter_ok;
//     use futures::future::{ok, poll_fn};
//
//     #[test]
//     fn requests() {
//         let rng = StdGen::new(rand::thread_rng(), 20);
//         let mut quickcheck = QuickCheck::new().gen(rng).tests(100);
//         quickcheck.quickcheck(test_requests as
//                               fn(usize,
//                                  usize,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>)
//                                  -> bool);
//     }
//
//     fn test_requests(buf_size_a: usize,
//                      buf_size_b: usize,
//                      write_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
//                      read_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
//                      write_ops_b: PartialWithErrors<GenInterruptedWouldBlock>,
//                      read_ops_b: PartialWithErrors<GenInterruptedWouldBlock>)
//                      -> bool {
//         let (writer_a, reader_a) = ring_buffer(buf_size_a + 1);
//         let writer_a = PartialAsyncWrite::new(writer_a, write_ops_a);
//         let reader_a = PartialAsyncRead::new(reader_a, read_ops_a);
//
//         let (writer_b, reader_b) = ring_buffer(buf_size_b + 1);
//         let writer_b = PartialAsyncWrite::new(writer_b, write_ops_b);
//         let reader_b = PartialAsyncRead::new(reader_b, read_ops_b);
//
//         let (a_in, mut a_out) = packet_stream(reader_a, writer_b);
//         let (b_in, mut b_out) = packet_stream(reader_b, writer_a);
//
//         let echo = b_in.for_each(|incoming_packet| match incoming_packet {
//                                      IncomingPacket::Request(in_request) => {
//                 let data = in_request.packet().data().clone();
//                 in_request.respond(data, PacketType::Binary)
//             }
//                                      IncomingPacket::Duplex(_, _, _) => unreachable!(),
//                                  })
//             .and_then(|_| poll_fn(|| b_out.close()));
//
//         let consume_a = a_in.for_each(|_| ok(()));
//
//         let (req0, res0) = a_out.request([0], PacketType::Binary);
//         let (req1, res1) = a_out.request([1], PacketType::Binary);
//         let (req2, res2) = a_out.request([2], PacketType::Binary);
//
//         let send_all = req0.join3(req1, req2)
//             .and_then(|_| poll_fn(|| a_out.close()));
//
//         let receive_all = res0.join3(res1, res2)
//             .map(|(r0, r1, r2)| {
//                      return r0.data() == &vec![0u8].into_boxed_slice() && r0.is_buffer_packet() &&
//                             r1.data() == &vec![1u8].into_boxed_slice() &&
//                             r1.is_buffer_packet() &&
//                             r2.data() == &vec![2u8].into_boxed_slice() &&
//                             r2.is_buffer_packet();
//                  });
//
//         return echo.join4(consume_a, send_all, receive_all)
//                    .map(|(_, _, _, worked)| worked)
//                    .wait()
//                    .unwrap();
//     }
//
//     #[test]
//     fn duplexes() {
//         let rng = StdGen::new(rand::thread_rng(), 20);
//         let mut quickcheck = QuickCheck::new().gen(rng).tests(100);
//         quickcheck.quickcheck(test_duplexes as
//                               fn(usize,
//                                  usize,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>,
//                                  PartialWithErrors<GenInterruptedWouldBlock>)
//                                  -> bool);
//     }
//
//     fn test_duplexes(buf_size_a: usize,
//                      buf_size_b: usize,
//                      write_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
//                      read_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
//                      write_ops_b: PartialWithErrors<GenInterruptedWouldBlock>,
//                      read_ops_b: PartialWithErrors<GenInterruptedWouldBlock>)
//                      -> bool {
//         let (writer_a, reader_a) = ring_buffer(buf_size_a + 1);
//         let writer_a = PartialAsyncWrite::new(writer_a, write_ops_a);
//         let reader_a = PartialAsyncRead::new(reader_a, read_ops_a);
//
//         let (writer_b, reader_b) = ring_buffer(buf_size_b + 1);
//         let writer_b = PartialAsyncWrite::new(writer_b, write_ops_b);
//         let reader_b = PartialAsyncRead::new(reader_b, read_ops_b);
//
//         let (a_in, mut a_out) = packet_stream(reader_a, writer_b);
//         let (b_in, mut b_out) = packet_stream(reader_b, writer_a);
//
//         let echo =
//             b_in.for_each(|incoming_packet| match incoming_packet {
//                               IncomingPacket::Request(_) => unreachable!(),
//                               IncomingPacket::Duplex(_, sink, stream) => {
//                                   stream
//                                       .take_while(|packet| ok(!packet.is_end_packet()))
//                                       .map(|packet| {
//                                                (packet.into_data(), PacketType::Binary, false)
//                                            })
//                                       .forward(sink)
//                                       .map(|_| ())
//                               }
//                           })
//                 .and_then(move |_| poll_fn(move || b_out.close()));
//
//         let consume_a = a_in.for_each(|_| ok(()));
//
//         let (sink0_a, stream0_a) = a_out.duplex();
//         let (sink1_a, stream1_a) = a_out.duplex();
//         let (sink2_a, stream2_a) = a_out.duplex();
//
//         let send_0 =
//             sink0_a.send_all(iter_ok::<_, io::Error>(vec![(vec![0], PacketType::Binary, false),
//                                                           (vec![0], PacketType::Binary, false),
//                                                           (vec![42], PacketType::Binary, true)]));
//         let send_1 =
//             sink1_a.send_all(iter_ok::<_, io::Error>(vec![(vec![1], PacketType::Binary, false),
//                                                           (vec![1], PacketType::Binary, false),
//                                                           (vec![1], PacketType::Binary, false),
//                                                           (vec![43], PacketType::Binary, true)]));
//         let send_2 =
//             sink2_a.send_all(iter_ok::<_, io::Error>(vec![(vec![2], PacketType::Binary, false),
//                                                           (vec![2], PacketType::Binary, false),
//                                                           (vec![2], PacketType::Binary, false),
//                                                           (vec![2], PacketType::Binary, false),
//                                                           (vec![44], PacketType::Binary, true)]));
//         let send_all = send_0
//             .join3(send_1, send_2)
//             .and_then(move |_| poll_fn(move || a_out.close()));
//
//         let receive_0 =
//             stream0_a
//                 .take(1)
//                 .fold(false, |_, packet| {
//                     ok::<_, io::Error>(packet.into_data() == vec![0].into_boxed_slice())
//                 });
//         let receive_1 = stream1_a
//             .take(2)
//             .fold(true, |acc, packet| {
//                 ok::<_, io::Error>(acc && packet.into_data() == vec![1].into_boxed_slice())
//             });
//         let receive_2 = stream2_a
//             .take(3)
//             .fold(true, |acc, packet| {
//                 ok::<_, io::Error>(acc && packet.into_data() == vec![2].into_boxed_slice())
//             });
//         let receive_all = receive_0
//             .join3(receive_1, receive_2)
//             .map(|(a, b, c)| a && b && c);
//
//         return echo.join4(consume_a, send_all, receive_all)
//                    .map(|(_, _, _, worked)| worked)
//                    .wait()
//                    .unwrap();
//     }
// }
