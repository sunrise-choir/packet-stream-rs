use std::io;
use std::cell::RefCell;
use std::rc::Rc;
use std::i32::MAX;
use std::io::ErrorKind::UnexpectedEof;

use std::fmt::Debug;

use futures::{Stream, Poll, Future, Sink, AsyncSink, StartSend, Async, AndThen};
use futures::sink::Send;
use futures::stream::Fuse;
use tokio_io::{AsyncRead, AsyncWrite};
use multi_producer_sink::MPS;
use multi_consumer_stream::{DefaultMCS, KeyMCS};
use atm_async_utils::sink_futures::Close;

use codec::*;

/// An enumeration representing the different types a packet can have.
#[derive(Clone, Copy)]
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

/// Take ownership of an AsyncRead and an AsyncWrite to create the two halves of
/// a packet-stream.
///
/// `R` is the `AsyncRead` for reading bytes from the peer, `W` is the
/// `AsyncWrite` for writing bytes to the peer, and `B` is the type that is used
/// as input for sending data.
///
/// Note that the AsyncRead may be polled for data even after it has signalled
/// end of data. Only use AsyncReads that can correctly handle this.
pub fn packet_stream<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>>
    (r: R,
     w: W)
     -> (PsIn<R, W, B>, PsOut<R, W, B>) {
    let ps = Rc::new(RefCell::new(PS::new(r, w)));

    (PsIn(Rc::clone(&ps)), PsOut(ps))
}

/// Shared state between a PsIn, PsOut and requests, duplexes, etc.
struct PS<R, W, B>
    where R: AsyncRead
{
    sink: MPS<PSCodecSink<W, B>>,
    stream: DefaultMCS<Fuse<PSCodecStream<R>>, PacketId, fn(&DecodedPacket) -> PacketId>,
    // The id used for the next actively sent packet.
    id_counter: PacketId,
    // The next id to be accepted as an active packet from the peer.
    accepted_id: PacketId,
}

impl<R, W, B> PS<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    fn new(r: R, w: W) -> PS<R, W, B> {
        PS {
            sink: MPS::new(PSCodecSink::new(w)),
            stream: DefaultMCS::new(PSCodecStream::new(r).fuse(), DecodedPacket::id),
            id_counter: 1,
            accepted_id: 1,
        }
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

    fn poll(&mut self) -> Poll<Option<IncomingPacket<R, W, B>>, io::Error> {
        match try_ready!(self.stream.poll()) {
            Some(p) => {
                if p.id() == self.accepted_id {
                    self.increment_accepted();
                    if p.is_stream_packet() {
                        Ok(Async::Ready(Some(IncomingPacket::Duplex(
                            PsSink::new(self.sink.clone(), p.id() * -1),
                            PsStream::new(self.stream.key_handle(p.id()), Some(p))
                        ))))
                    } else {
                        Ok(Async::Ready(Some(IncomingPacket::Request(InRequest::new(p, self.sink.clone())))))
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

/// Allows sending packets to the peer.
pub struct PsOut<R: AsyncRead, W, B>(Rc<RefCell<PS<R, W, B>>>);

impl<R, W, B> PsOut<R, W, B>
    where R: AsyncRead,
          W: AsyncWrite,
          B: AsRef<[u8]>
{
    /// Send a request to the peer.
    ///
    /// The `SendRequest` Future must be polled to actually start sending the
    /// request. The `InResponse` Future can be polled to receive the response.
    pub fn request(&self, data: B, t: PsPacketType) -> (SendRequest<W, B>, InResponse<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (SendRequest::new(ps.sink.clone(), data, t, id),
         InResponse::new(ps.stream.key_handle(id * -1)))
    }

    /// Create a `PsDuplex`, a bidirectional channel multiplexed over the
    /// underlying sink/stream pair.
    pub fn duplex(&self) -> (PsSink<W, B>, PsStream<R>) {
        let mut ps = self.0.borrow_mut();

        let id = ps.next_id();
        (PsSink::new(ps.sink.clone(), id), PsStream::new(ps.stream.key_handle(id * -1), None))
    }

    /// Close the packet-stream, indicating that no more packets will be sent.
    ///
    /// This does not immediately close if there are still unsent requests or
    /// active `PsDuplex`es. In that case, the closing happens when the last
    /// request is sent or the last duplex is closed.
    pub fn close(&mut self) -> Poll<(), io::Error> {
        self.0.borrow_mut().sink.close()
    }
}

/// A request initated by this packet-stream.
///
/// Poll it to actually start sending the request.
pub struct SendRequest<W: AsyncWrite, B: AsRef<[u8]>>(AndThen<Send<MPS<PSCodecSink<W, B>>>,
                                                               Close<MPS<PSCodecSink<W, B>>>,
                                                               fn(MPS<PSCodecSink<W, B>>)
                                                                  -> Close<MPS<PSCodecSink<W,
                                                                                            B>>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> SendRequest<W, B> {
    fn new(sink_handle: MPS<PSCodecSink<W, B>>,
           data: B,
           t: PsPacketType,
           id: PacketId)
           -> SendRequest<W, B> {
        SendRequest(sink_handle
                        .send((data,
                               MetaData {
                                   flags: t.flags(),
                                   id,
                               }))
                        .and_then(|s| Close::new(s)))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for SendRequest<W, B> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

/// A response that will be received from the peer.
pub struct InResponse<R: AsyncRead>(KeyMCS<Fuse<PSCodecStream<R>>,
                                            PacketId,
                                            fn(&DecodedPacket) -> PacketId>);

impl<R: AsyncRead> InResponse<R> {
    fn new(stream_handle: KeyMCS<Fuse<PSCodecStream<R>>,
                                 PacketId,
                                 fn(&DecodedPacket) -> PacketId>)
           -> InResponse<R> {
        InResponse(stream_handle)
    }
}

impl<R: AsyncRead> Future for InResponse<R> {
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

/// The sink half of a duplex multiplexed over the packet-stream.
pub struct PsSink<W: AsyncWrite, B: AsRef<[u8]>> {
    sink: MPS<PSCodecSink<W, B>>,
    id: PacketId,
}

impl<W, B> PsSink<W, B>
    where W: AsyncWrite,
          B: AsRef<[u8]>
{
    fn new(sink: MPS<PSCodecSink<W, B>>, id: PacketId) -> PsSink<W, B> {
        PsSink { sink, id }
    }
}

impl<W, B> Sink for PsSink<W, B>
    where W: AsyncWrite,
          B: AsRef<[u8]> + Debug
{
    /// If the `bool` is true, the end/error flag gets set.
    type SinkItem = (B, PsPacketType, bool);
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let mut flags = item.1.flags() | STREAM;
        if item.2 {
            flags |= END;
        }

        match self.sink
                  .start_send((item.0, MetaData { flags, id: self.id })) {
            Ok(AsyncSink::NotReady((bytes, _))) => Ok(AsyncSink::NotReady((bytes, item.1, item.2))),
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Err(e) => Err(e),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.poll_complete()
    }

    /// This only closes the packet-stream if necessary, but it does not
    /// automatically send a packet signalling the end of the substream.
    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.close()
    }
}

/// The stream half of a duplex multiplexed over the packet-stream.
pub struct PsStream<R: AsyncRead> {
    stream: KeyMCS<Fuse<PSCodecStream<R>>, PacketId, fn(&DecodedPacket) -> PacketId>,
    initial: Option<DecodedPacket>,
}

impl<R: AsyncRead> PsStream<R> {
    fn new(stream: KeyMCS<Fuse<PSCodecStream<R>>, PacketId, fn(&DecodedPacket) -> PacketId>,
           initial: Option<DecodedPacket>)
           -> PsStream<R> {
        PsStream { stream, initial }
    }
}

/// Note that the stream never emits `Ok(Async::Ready(None))`, this needs to be
/// implemented on top of it.
impl<R: AsyncRead> Stream for PsStream<R> {
    type Item = DecodedPacket;
    type Error = io::Error;

    /// Note that the stream never emits `Ok(Async::Ready(None))`, this needs to be
    /// implemented on top of it.
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.initial.take() {
            Some(p) => Ok(Async::Ready(Some(p))),
            None => {
                match try_ready!(self.stream.poll()) {
                    Some(packet) => Ok(Async::Ready(Some(packet))),
                    None => {
                        Err(io::Error::new(UnexpectedEof,
                                           "packet stream closed while substream was waiting for items"))
                    }
                }
            }
        }
    }
}

/// A stream of incoming requests from the peer.
pub struct PsIn<R: AsyncRead, W, B>(Rc<RefCell<PS<R, W, B>>>);

impl<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> Stream for PsIn<R, W, B> {
    type Item = IncomingPacket<R, W, B>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.borrow_mut().poll()
    }
}

/// An incoming packet, initiated by the peer.
pub enum IncomingPacket<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> {
    /// An incoming request.
    Request(InRequest<W, B>),
    /// A duplex connection initiated by the peer.
    Duplex(PsSink<W, B>, PsStream<R>),
}

/// A request initated by the peer. Drop to ignore it, or use `respond` to send
/// a response.
pub struct InRequest<W, B> {
    packet: DecodedPacket,
    sink: MPS<PSCodecSink<W, B>>,
}

impl<W, B> InRequest<W, B> {
    fn new(packet: DecodedPacket, sink: MPS<PSCodecSink<W, B>>) -> InRequest<W, B> {
        InRequest { packet, sink }
    }

    /// Returns the packet corresponding to this incoming request.
    pub fn packet(&self) -> &DecodedPacket {
        &self.packet
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> InRequest<W, B> {
    /// Returns a Future which completes once the given packet has been sent to
    /// the peer.
    pub fn respond(self, bytes: B, t: PsPacketType) -> SendResponse<W, B> {
        SendResponse::new(self.sink, self.packet.id() * -1, bytes, t)
    }

    /// Returns a Future which completes once the given packet has been sent to
    /// the peer. The error flag is set in the header.
    pub fn respond_error(self, bytes: B, t: PsPacketType) -> SendResponseError<W, B> {
        SendResponseError::new(self.sink, self.packet.id() * -1, bytes, t)
    }
}

/// Future that completes when the given bytes have been sent as a response to
/// the peer.
pub struct SendResponse<W: AsyncWrite, B: AsRef<[u8]>>(AndThen<Send<MPS<PSCodecSink<W, B>>>,
                                                                Close<MPS<PSCodecSink<W, B>>>,
                                                                fn(MPS<PSCodecSink<W, B>>)
                                                                   -> Close<MPS<PSCodecSink<W,
                                                                                             B>>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> SendResponse<W, B> {
    fn new(sink_handle: MPS<PSCodecSink<W, B>>,
           id: PacketId,
           data: B,
           t: PsPacketType)
           -> SendResponse<W, B> {
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

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for SendResponse<W, B> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

/// Future that completes when the given bytes have been sent as a response to
/// the peer, with the error flag set.
pub struct SendResponseError<W: AsyncWrite, B: AsRef<[u8]>>(AndThen<Send<MPS<PSCodecSink<W, B>>>,
                                                                Close<MPS<PSCodecSink<W, B>>>,
                                                                fn(MPS<PSCodecSink<W, B>>)
                                                                   -> Close<MPS<PSCodecSink<W,
                                                                                             B>>>>);

impl<W: AsyncWrite, B: AsRef<[u8]>> SendResponseError<W, B> {
    fn new(sink_handle: MPS<PSCodecSink<W, B>>,
           id: PacketId,
           data: B,
           t: PsPacketType)
           -> SendResponseError<W, B> {
        debug_assert!(id < 0);

        SendResponseError(sink_handle
                              .send((data,
                                     MetaData {
                                         flags: t.flags() | END,
                                         id: id,
                                     }))
                              .and_then(|s| Close::new(s)))
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for SendResponseError<W, B> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());
        Ok(Async::Ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use partial_io::{PartialAsyncRead, PartialAsyncWrite, PartialWithErrors};
    use partial_io::quickcheck_types::GenInterruptedWouldBlock;
    use quickcheck::{QuickCheck, StdGen};
    use async_ringbuffer::*;
    use rand;
    use futures::stream::iter_ok;
    use futures::future::{ok, poll_fn};

    #[test]
    fn requests() {
        let rng = StdGen::new(rand::thread_rng(), 20);
        let mut quickcheck = QuickCheck::new().gen(rng).tests(100);
        quickcheck.quickcheck(test_requests as
                              fn(usize,
                                 usize,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>)
                                 -> bool);
    }

    fn test_requests(buf_size_a: usize,
                     buf_size_b: usize,
                     write_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
                     read_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
                     write_ops_b: PartialWithErrors<GenInterruptedWouldBlock>,
                     read_ops_b: PartialWithErrors<GenInterruptedWouldBlock>)
                     -> bool {
        let (writer_a, reader_a) = ring_buffer(buf_size_a + 1);
        let writer_a = PartialAsyncWrite::new(writer_a, write_ops_a);
        let reader_a = PartialAsyncRead::new(reader_a, read_ops_a);

        let (writer_b, reader_b) = ring_buffer(buf_size_b + 1);
        let writer_b = PartialAsyncWrite::new(writer_b, write_ops_b);
        let reader_b = PartialAsyncRead::new(reader_b, read_ops_b);

        let (a_in, mut a_out) = packet_stream(reader_a, writer_b);
        let (b_in, mut b_out) = packet_stream(reader_b, writer_a);

        let echo = b_in.for_each(|incoming_packet| match incoming_packet {
                                     IncomingPacket::Request(in_request) => {
                let data = in_request.packet().data().clone();
                in_request.respond(data, PsPacketType::Binary)
            }
                                     IncomingPacket::Duplex(_, _) => unreachable!(),
                                 })
            .and_then(|_| poll_fn(|| b_out.close()));

        let consume_a = a_in.for_each(|_| ok(()));

        let (req0, res0) = a_out.request([0], PsPacketType::Binary);
        let (req1, res1) = a_out.request([1], PsPacketType::Binary);
        let (req2, res2) = a_out.request([2], PsPacketType::Binary);

        let send_all = req0.join3(req1, req2)
            .and_then(|_| poll_fn(|| a_out.close()));

        let receive_all = res0.join3(res1, res2)
            .map(|(r0, r1, r2)| {
                     return r0.data() == &vec![0u8] && r0.is_buffer_packet() &&
                            r1.data() == &vec![1u8] &&
                            r1.is_buffer_packet() &&
                            r2.data() == &vec![2u8] &&
                            r2.is_buffer_packet();
                 });

        return echo.join4(consume_a, send_all, receive_all)
                   .map(|(_, _, _, worked)| worked)
                   .wait()
                   .unwrap();
    }

    #[test]
    fn duplexes() {
        let rng = StdGen::new(rand::thread_rng(), 20);
        let mut quickcheck = QuickCheck::new().gen(rng).tests(100);
        quickcheck.quickcheck(test_duplexes as
                              fn(usize,
                                 usize,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>)
                                 -> bool);
    }

    fn test_duplexes(buf_size_a: usize,
                     buf_size_b: usize,
                     write_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
                     read_ops_a: PartialWithErrors<GenInterruptedWouldBlock>,
                     write_ops_b: PartialWithErrors<GenInterruptedWouldBlock>,
                     read_ops_b: PartialWithErrors<GenInterruptedWouldBlock>)
                     -> bool {
        let (writer_a, reader_a) = ring_buffer(buf_size_a + 1);
        let writer_a = PartialAsyncWrite::new(writer_a, write_ops_a);
        let reader_a = PartialAsyncRead::new(reader_a, read_ops_a);

        let (writer_b, reader_b) = ring_buffer(buf_size_b + 1);
        let writer_b = PartialAsyncWrite::new(writer_b, write_ops_b);
        let reader_b = PartialAsyncRead::new(reader_b, read_ops_b);

        let (a_in, mut a_out) = packet_stream(reader_a, writer_b);
        let (b_in, mut b_out) = packet_stream(reader_b, writer_a);

        let echo =
            b_in.for_each(|incoming_packet| match incoming_packet {
                              IncomingPacket::Request(_) => unreachable!(),
                              IncomingPacket::Duplex(sink, stream) => {
                                  stream
                                      .take_while(|packet| ok(!packet.is_end_packet()))
                                      .map(|packet| {
                                               (packet.into_data(), PsPacketType::Binary, false)
                                           })
                                      .forward(sink)
                                      .map(|_| ())
                              }
                          })
                .and_then(move |_| poll_fn(move || b_out.close()));

        let consume_a = a_in.for_each(|_| ok(()));

        let (sink0_a, stream0_a) = a_out.duplex();
        let (sink1_a, stream1_a) = a_out.duplex();
        let (sink2_a, stream2_a) = a_out.duplex();

        let send_0 =
            sink0_a.send_all(iter_ok::<_, io::Error>(vec![(vec![0], PsPacketType::Binary, false),
                                                          (vec![42], PsPacketType::Binary, true)]));
        let send_1 =
            sink1_a.send_all(iter_ok::<_, io::Error>(vec![(vec![1], PsPacketType::Binary, false),
                                                          (vec![1], PsPacketType::Binary, false),
                                                          (vec![43], PsPacketType::Binary, true)]));
        let send_2 =
            sink2_a.send_all(iter_ok::<_, io::Error>(vec![(vec![2], PsPacketType::Binary, false),
                                                          (vec![2], PsPacketType::Binary, false),
                                                          (vec![2], PsPacketType::Binary, false),
                                                          (vec![44], PsPacketType::Binary, true)]));
        let send_all = send_0
            .join3(send_1, send_2)
            .and_then(move |_| poll_fn(move || a_out.close()));

        let receive_0 = stream0_a
            .take(1)
            .fold(false,
                  |_, packet| ok::<_, io::Error>(packet.into_data() == vec![0]));
        let receive_1 =
            stream1_a
                .take(2)
                .fold(true,
                      |acc, packet| ok::<_, io::Error>(acc && packet.into_data() == vec![1]));
        let receive_2 =
            stream2_a
                .take(3)
                .fold(true,
                      |acc, packet| ok::<_, io::Error>(acc && packet.into_data() == vec![2]));
        let receive_all = receive_0
            .join3(receive_1, receive_2)
            .map(|(a, b, c)| a && b && c);

        return echo.join4(consume_a, send_all, receive_all)
                   .map(|(_, _, _, worked)| worked)
                   .wait()
                   .unwrap();
    }
}
