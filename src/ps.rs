use std::marker::PhantomData;
use std::io;
use std::cell::RefCell;
use std::rc::Rc;
use std::i32::MAX;

use futures::{Stream, Poll, Future, Sink, AsyncSink, StartSend, Async, AndThen};
use futures::sink::Send;
use futures::stream::StreamFuture;
use futures::future::{Map, MapErr};
use tokio_io::{AsyncRead, AsyncWrite};
use void::Void;
use multi_producer_sink::MPS;
use multi_consumer_stream::{DefaultMCS, KeyMCS};
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

/// Take ownership of an AsyncRead and an AsyncWrite to create the two halves of
/// a packet-stream.
///
/// `R` is the `AsyncRead` for reading bytes from the peer, `W` is the
/// `AsyncWrite` for writing bytes to the peer, and `B` is the type that is used
/// as input for sending data.
///
/// Note that the AsyncRead may be polled for data even after it has signalled
/// end of data. Only use AsyncReads that can correctly handle this.
pub fn packet_stream<R: AsyncRead, W, B: AsRef<[u8]>>(r: R,
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
    stream: DefaultMCS<PSCodecStream<R>, PacketId, fn(&DecodedPacket) -> PacketId>,
    // The id used for the next actively sent packet.
    id_counter: PacketId,
    // The next id to be accepted as an active packet from the peer.
    accepted_id: PacketId,
}

impl<R, W, B> PS<R, W, B>
    where R: AsyncRead,
          B: AsRef<[u8]>
{
    fn new(r: R, w: W) -> PS<R, W, B> {
        PS {
            sink: MPS::new(PSCodecSink::new(w)),
            stream: DefaultMCS::new(PSCodecStream::new(r), DecodedPacket::id),
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

    fn poll(&mut self) -> Poll<Option<IncomingPacket<W, B>>, io::Error> {
        match try_ready!(self.stream.poll()) {
            Some(p) => {
                if p.id() == self.accepted_id {
                    self.increment_accepted();
                    if p.is_stream_packet() {
                        unimplemented!() // TODO
                        // Ok(Async::Ready(Some(IncomingPacket::Duplex(PsDuplex::new(p,
                        //                                             self.stream_handle
                        //                                                 .key_handle(p.id()))))))
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

    // TODO create duplexes

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
pub struct InResponse<R: AsyncRead>(KeyMCS<PSCodecStream<R>,
                                            PacketId,
                                            fn(&DecodedPacket) -> PacketId>);

impl<R: AsyncRead> InResponse<R> {
    fn new(stream_handle: KeyMCS<PSCodecStream<R>, PacketId, fn(&DecodedPacket) -> PacketId>)
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

/// A stream of incoming requests from the peer.
pub struct PsIn<R: AsyncRead, W, B>(Rc<RefCell<PS<R, W, B>>>);

impl<R: AsyncRead, W: AsyncWrite, B: AsRef<[u8]>> Stream for PsIn<R, W, B> {
    type Item = IncomingPacket<W, B>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.borrow_mut().poll()
    }
}

/// An incoming packet, initiated by the peer.
pub enum IncomingPacket<W, B> {
    /// An incoming request.
    Request(InRequest<W, B>),
    /// A duplex connection initiated by the peer.
    Duplex(PsDuplex<B>),
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
    fn packet(&self) -> &DecodedPacket {
        &self.packet
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> InRequest<W, B> {
    /// Returns a Future which completes once the given packet has been sent to
    /// the peer.
    fn respond(self, bytes: B, t: PsPacketType) -> SendResponse<W, B> {
        SendResponse::new(self.sink, self.packet.id() * -1, bytes, t)
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::ErrorKind::InvalidData;
    use std::io::Write;

    use partial_io::{PartialAsyncRead, PartialAsyncWrite, PartialWithErrors};
    use partial_io::quickcheck_types::GenInterruptedWouldBlock;
    use quickcheck::{QuickCheck, StdGen};
    use async_ringbuffer::*;
    use rand;
    use futures::stream::iter_ok;
    use futures::future::{join_all, ok, poll_fn};

    #[test]
    fn requests() {
        let rng = StdGen::new(rand::thread_rng(), 4);
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
                                     IncomingPacket::Duplex(_) => unreachable!(),
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
}
