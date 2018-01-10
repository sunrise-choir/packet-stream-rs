// Implements [packet-stream-codec](https://github.com/dominictarr/packet-stream-codec).

use std::io::Error;
use std::io::ErrorKind::{WouldBlock, Interrupted, WriteZero, UnexpectedEof, InvalidData};
use std::u32::MAX;
use std::mem::transmute;
use std::slice::from_raw_parts_mut;

use tokio_io::{AsyncWrite, AsyncRead};
use futures::{Future, Poll, Stream, Sink, StartSend, AsyncSink, Async};
use futures::Async::{Ready, NotReady};

use ps::PsPacketType;

pub type PacketId = i32;

pub struct MetaData {
    flags: u8,
    id: PacketId,
}

// Flags
static STREAM: u8 = 0b0000_1000;
static END: u8 = 0b0000_0100;
static TYPE: u8 = 0b0000_0011;

static TYPE_BINARY: u8 = 0;
static TYPE_STRING: u8 = 1;
static TYPE_JSON: u8 = 2;
static TYPE_INVALID: u8 = 3;

/// A Future used to write an entire packet into an AsyncWrite.
pub struct WritePacket<W, B> {
    write: Option<W>,
    packet: B,
    state: WritePacketState,
}

impl<W, B: AsRef<[u8]>> WritePacket<W, B> {
    /// Create a new WritePacket future. Panics if the buffer is larger than
    /// the maximum packet size `std::u32::MAX`.
    ///
    /// The flags are set via the type-level parameters T, S and E.
    pub fn new(write: W, packet: B, id: PacketId, flags: u8) -> WritePacket<W, B> {
        if packet.as_ref().len() > MAX as usize {
            panic!("Packet too large for packet-stream");
        }

        WritePacket {
            write: Some(write),
            packet,
            state: WritePacketState::WriteFlags(MetaData {
                                                    flags: flags.to_be(),
                                                    id: id.to_be(),
                                                }),
        }
    }

    pub fn into_inner(self) -> W {
        self.write
            .expect("Called into_inner on WritePacket future after completion")
    }
}

// State for the WritePacket future.
enum WritePacketState {
    WriteFlags(MetaData),
    WriteLength(PacketId, u8), // u8 signifies how many bytes of the length have been written
    WriteId(PacketId, u8), // u8 signifies how many bytes of the id have been written
    WritePacket(u32), // u32 signifies how many bytes of the packet have been written
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Future for WritePacket<W, B> {
    type Item = W;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut w = self.write
            .take()
            .expect("Polled WritePacket after completion");

        match self.state {
            WritePacketState::WriteFlags(MetaData { flags, id }) => {
                loop {
                    match w.write(&[flags]) {
                        Ok(0) => {
                            return Err(Error::new(WriteZero, "failed to write packet flags"));
                        }
                        Ok(_) => {
                            self.state = WritePacketState::WriteLength(id, 0);
                            self.write = Some(w);
                            return self.poll();
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.write = Some(w);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }

            WritePacketState::WriteLength(id, initial_offset) => {
                let len_bytes =
                    unsafe { transmute::<_, [u8; 4]>((self.packet.as_ref().len() as u32).to_be()) };
                let mut offset = initial_offset;
                while offset < 4 {
                    match w.write(&len_bytes[offset as usize..]) {
                        Ok(0) => {
                            return Err(Error::new(WriteZero, "failed to write packet length"));
                        }
                        Ok(written) => {
                            offset += written as u8;
                            self.state = WritePacketState::WriteLength(id, offset);
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.write = Some(w);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                self.state = WritePacketState::WriteId(id, 0);
                self.write = Some(w);
                return self.poll();
            }

            WritePacketState::WriteId(id, initial_offset) => {
                let id_bytes = unsafe { transmute::<_, [u8; 4]>(id) };
                let mut offset = initial_offset;
                while offset < 4 {
                    match w.write(&id_bytes[offset as usize..]) {
                        Ok(0) => {
                            return Err(Error::new(WriteZero, "failed to write packet id"));
                        }
                        Ok(written) => {
                            offset += written as u8;
                            self.state = WritePacketState::WriteId(id, offset);
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.write = Some(w);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                self.state = WritePacketState::WritePacket(0);
                self.write = Some(w);
                return self.poll();
            }

            WritePacketState::WritePacket(initial_offset) => {
                let mut offset = initial_offset;
                while (offset as usize) < self.packet.as_ref().len() {
                    match w.write(&self.packet.as_ref()[offset as usize..]) {
                        Ok(0) => {
                            return Err(Error::new(WriteZero, "failed to write packet data"));
                        }
                        Ok(written) => {
                            offset += written as u32;
                            self.state = WritePacketState::WritePacket(offset);
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.write = Some(w);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                return Ok(Ready(w));
            }
        }
    }
}

/// Future to write an end of packet-stream header.
pub struct WriteZeros<W>(u8, Option<W>);
//The state is how many zero bytes have already been written.

impl<W> WriteZeros<W> {
    /// Create a new WriteZeros future.
    pub fn new(w: W) -> WriteZeros<W> {
        WriteZeros(0, Some(w))
    }

    pub fn into_inner(self) -> W {
        self.1
            .expect("Called into_inner on WriteZeros future after completion")
    }
}

impl<W: AsyncWrite> Future for WriteZeros<W> {
    type Item = W;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let zeros = [0u8, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut offset = self.0;
        let mut w = self.1
            .take()
            .expect("Polled WriteZeros after completion");

        while offset < 9 {
            match w.write(&zeros[offset as usize..]) {
                Ok(0) => {
                    return Err(Error::new(WriteZero, "failed to write end-of-stream header"));
                }
                Ok(written) => {
                    offset += written as u8;
                    self.0 = offset;
                }
                Err(ref e) if e.kind() == WouldBlock => {
                    self.1 = Some(w);
                    return Ok(NotReady);
                }
                Err(ref e) if e.kind() == Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        return Ok(Ready(w));
    }
}

enum SinkState<W, B> {
    Waiting(W),
    Writing(WritePacket<W, B>),
    Ending(WriteZeros<W>),
    ShuttingDown(W),
}

// From AsyncWrite to Sink<(B, MetaData)>
pub struct PSCodecSink<W, B> {
    state: Option<SinkState<W, B>>,
}

impl<W, B: AsRef<[u8]>> PSCodecSink<W, B> {
    pub fn new(write: W) -> PSCodecSink<W, B> {
        PSCodecSink { state: Some(SinkState::Waiting(write)) }
    }

    pub fn into_inner(self) -> W {
        match self.state.unwrap() {
            SinkState::Waiting(w) => w,
            SinkState::Writing(wp) => wp.into_inner(),
            SinkState::Ending(wz) => wz.into_inner(),
            SinkState::ShuttingDown(w) => w,
        }
    }

    fn waiting(&mut self, w: W) {
        self.state = Some(SinkState::Waiting(w));
    }

    fn writing(&mut self, wp: WritePacket<W, B>) {
        self.state = Some(SinkState::Writing(wp));
    }

    fn ending(&mut self, wz: WriteZeros<W>) {
        self.state = Some(SinkState::Ending(wz));
    }

    fn shutting_down(&mut self, w: W) {
        self.state = Some(SinkState::ShuttingDown(w));
    }
}

impl<W: AsyncWrite, B: AsRef<[u8]>> Sink for PSCodecSink<W, B> {
    type SinkItem = (B, MetaData);
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.state.take().unwrap() {
            SinkState::Waiting(w) => {
                self.state =
                    Some(SinkState::Writing(WritePacket::new(w, item.0, item.1.id, item.1.flags)));
                return Ok(AsyncSink::Ready);
            }
            SinkState::Writing(mut wp) => {
                match wp.poll() {
                    Ok(Async::Ready(w)) => {
                        self.waiting(w);
                        return self.start_send(item);
                    }
                    Ok(Async::NotReady) => {
                        self.writing(wp);
                        return Ok(AsyncSink::NotReady(item));
                    }
                    Err(e) => {
                        self.writing(wp);
                        return Err(e);
                    }
                }
            }
            SinkState::Ending(_) => panic!("Wrote to packet-stream after closing"),
            SinkState::ShuttingDown(_) => panic!("Wrote to packet-stream after closing"),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self.state.take().unwrap() {
            SinkState::Waiting(mut w) => {
                loop {
                    match w.flush() {
                        Ok(_) => {
                            self.waiting(w);
                            return Ok(Async::Ready(()));
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.waiting(w);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => {
                            self.waiting(w);
                            return Err(e);
                        }
                    }
                }
            }
            SinkState::Writing(mut wp) => {
                match wp.poll() {
                    Ok(Async::Ready(w)) => {
                        self.waiting(w);
                        return self.poll_complete();
                    }
                    Ok(Async::NotReady) => {
                        self.writing(wp);
                        return Ok(Async::NotReady);
                    }
                    Err(e) => {
                        self.writing(wp);
                        return Err(e);
                    }
                }
            }
            SinkState::Ending(wz) => {
                self.ending(wz);
                self.close()
            }
            SinkState::ShuttingDown(w) => {
                self.shutting_down(w);
                self.close()
            }
        }
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        match self.state.take().unwrap() {
            SinkState::Waiting(w) => {
                self.ending(WriteZeros::new(w));
                return self.close();
            }
            SinkState::Writing(mut wp) => {
                match wp.poll() {
                    Ok(Async::Ready(w)) => {
                        self.ending(WriteZeros::new(w));
                        return self.close();
                    }
                    Ok(Async::NotReady) => {
                        self.writing(wp);
                        return Ok(Async::NotReady);
                    }
                    Err(e) => {
                        self.writing(wp);
                        return Err(e);
                    }
                }
            }
            SinkState::Ending(mut wz) => {
                match wz.poll() {
                    Ok(Async::Ready(w)) => {
                        self.shutting_down(w);
                        return self.close();
                    }
                    Ok(Async::NotReady) => {
                        self.ending(wz);
                        return Ok(Async::NotReady);
                    }
                    Err(e) => {
                        self.ending(wz);
                        return Err(e);
                    }
                }
            }
            SinkState::ShuttingDown(mut w) => {
                match w.shutdown() {
                    Ok(Async::Ready(_)) => {
                        self.shutting_down(w);
                        return Ok(Async::Ready(()));
                    }
                    Ok(Async::NotReady) => {
                        self.shutting_down(w);
                        return Ok(Async::NotReady);
                    }
                    Err(e) => {
                        self.shutting_down(w);
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// A packet decoded an AsyncRead.
#[derive(Debug)]
pub struct DecodedPacket {
    flags: u8,
    id: PacketId,
    bytes: Vec<u8>,
}

impl DecodedPacket {
    /// Returns true if the stream flag of the packet is set.
    pub fn is_stream_packet(&self) -> bool {
        self.flags & STREAM != 0
    }

    /// Returns true if the end flag of the packet is set.
    pub fn is_end_packet(&self) -> bool {
        self.flags & END != 0
    }

    /// Returns true if the type flags signal a buffer.
    pub fn is_buffer_packet(&self) -> bool {
        self.flags & TYPE == TYPE_BINARY
    }

    /// Returns true if the type flags signal a string.
    pub fn is_string_packet(&self) -> bool {
        self.flags & TYPE == TYPE_STRING
    }

    /// Returns true if the type flags signal json.
    pub fn is_json_packet(&self) -> bool {
        self.flags & TYPE == TYPE_JSON
    }

    /// Returns the id of the packet.
    pub fn id(&self) -> PacketId {
        self.id
    }

    /// Returns a reference to the packet's data.
    pub fn data(&self) -> &Vec<u8> {
        &self.bytes
    }

    /// Returns a mutable reference to the packet's data.
    pub fn data_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Consumes the packet and gives ownership of its data.
    pub fn into_data(self) -> Vec<u8> {
        self.bytes
    }
}

/// A Future used to read a packet from an AsyncRead
pub struct ReadPacket<R> {
    read: Option<R>,
    packet: Option<DecodedPacket>,
    state: ReadPacketState,
}

impl<R> ReadPacket<R> {
    /// Create a new ReadPacket future to read a packet from an AsyncRead.
    pub fn new(read: R) -> ReadPacket<R> {
        ReadPacket {
            read: Some(read),
            packet: Some(DecodedPacket {
                             flags: 0,
                             id: 0,
                             bytes: Vec::new(),
                         }),
            state: ReadPacketState::ReadFlags,
        }
    }
}

enum ReadPacketState {
    ReadFlags,
    ReadLength(u8, [u8; 4]),
    ReadId(u8, u32, [u8; 4]),
    ReadBytes(u32),
}

impl<R: AsyncRead> Future for ReadPacket<R> {
    /// Yields `(None, read)` when encountering an end-of-packet-stream header.
    type Item = (Option<DecodedPacket>, R);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut r = self.read
            .take()
            .expect("Polled ReadPacket after completion");
        let mut packet = self.packet.take().unwrap();

        match self.state {
            ReadPacketState::ReadFlags => {
                let mut read_buf = [0u8; 1];
                loop {
                    match r.read(&mut read_buf) {
                        Ok(0) => {
                            return Err(Error::new(UnexpectedEof, "failed to read packet flags"));
                        }
                        Ok(_) => {
                            packet.flags = read_buf[0];
                            if (packet.flags & TYPE) == TYPE_INVALID {
                                return Err(Error::new(InvalidData,
                                                      "read packet with invalid type flag"));
                            }

                            self.state = ReadPacketState::ReadLength(0, [0; 4]);
                            self.read = Some(r);
                            self.packet = Some(packet);
                            return self.poll();
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.read = Some(r);
                            self.packet = Some(packet);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }

            ReadPacketState::ReadLength(initial_offset, mut length_buf) => {
                let mut offset = initial_offset;
                while offset < 4 {
                    match r.read(&mut length_buf[offset as usize..]) {
                        Ok(0) => {
                            return Err(Error::new(UnexpectedEof, "failed to read packet length"));
                        }
                        Ok(read) => {
                            offset += read as u8;
                            self.state = ReadPacketState::ReadLength(offset, length_buf);
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.read = Some(r);
                            self.packet = Some(packet);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                let length = u32::from_be(unsafe { transmute::<[u8; 4], u32>(length_buf) });
                self.state = ReadPacketState::ReadId(0, length, [0u8; 4]);
                self.read = Some(r);
                self.packet = Some(packet);
                return self.poll();
            }

            ReadPacketState::ReadId(initial_offset, length, mut id_buf) => {
                let mut offset = initial_offset;
                while offset < 4 {
                    match r.read(&mut id_buf[offset as usize..]) {
                        Ok(0) => {
                            return Err(Error::new(UnexpectedEof, "failed to read packet id"));
                        }
                        Ok(read) => {
                            offset += read as u8;
                            self.state = ReadPacketState::ReadId(offset, length, id_buf);
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.read = Some(r);
                            self.packet = Some(packet);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                packet.id = i32::from_be(unsafe { transmute::<[u8; 4], i32>(id_buf) });

                if (length == 0) && (packet.id == 0) && (packet.flags == 0) {
                    return Ok(Ready((None, r)));
                }

                self.state = ReadPacketState::ReadBytes(length);
                packet.bytes.reserve_exact(length as usize);
                self.read = Some(r);
                self.packet = Some(packet);
                return self.poll();
            }

            ReadPacketState::ReadBytes(length) => {
                let mut old_len = packet.bytes.len();

                let capacity = packet.bytes.capacity();
                let data_ptr = packet.bytes.as_mut_slice().as_mut_ptr();
                let data_slice = unsafe { from_raw_parts_mut(data_ptr, capacity) };

                while old_len < length as usize {
                    match r.read(&mut data_slice[old_len..]) {
                        Ok(0) => {
                            return Err(Error::new(UnexpectedEof,
                                                  "failed to read whole packet content"));
                        }
                        Ok(read) => {
                            unsafe { packet.bytes.set_len(old_len + read) };
                            old_len += read;
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.read = Some(r);
                            self.packet = Some(packet);
                            return Ok(NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                return Ok(Ready((Some(packet), r)));
            }
        }
    }
}

// From AsyncRead to Stream<DecodedPacket>
pub struct PSCodecStream<R> {
    // state: Option<StreamState<R>>,
    state: Option<ReadPacket<R>>,
}

impl<R> PSCodecStream<R> {
    pub fn new(read: R) -> PSCodecStream<R> {
        // PSCodecStream { state: Some(StreamState::Waiting(read)) }
        PSCodecStream { state: Some(ReadPacket::new(read)) }
    }
}

impl<R: AsyncRead> Stream for PSCodecStream<R> {
    type Item = DecodedPacket;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut rp = self.state
            .take()
            .expect("Polled packet-stream after it closed");
        match rp.poll() {
            Ok(Async::Ready((Some(decoded_packet), r))) => {
                self.state = Some(ReadPacket::new(r));
                return Ok(Async::Ready(Some(decoded_packet)));
            }
            Ok(Async::Ready((None, r))) => {
                self.state = Some(ReadPacket::new(r));
                return Ok(Async::Ready(None));
            }
            Ok(Async::NotReady) => {
                self.state = Some(rp);
                return Ok(Async::NotReady);
            }
            Err(e) => {
                self.state = Some(rp);
                return Err(e);
            }
        }
    }
}

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

    #[test]
    fn codec() {
        let rng = StdGen::new(rand::thread_rng(), 2000);
        let mut quickcheck = QuickCheck::new().gen(rng).tests(100);
        quickcheck.quickcheck(test_codec as
                              fn(usize,
                                 i32,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 Vec<u8>)
                                 -> bool);
    }

    fn test_codec(buf_size: usize,
                  id: i32,
                  write_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                  read_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                  data: Vec<u8>)
                  -> bool {
        let expected_data = data.clone();

        let (writer, reader) = ring_buffer(buf_size + 1);
        let writer = PartialAsyncWrite::new(writer, write_ops);
        let reader = PartialAsyncRead::new(reader, read_ops);
        let write_packet = WritePacket::new(writer, data, id, 0b0000_1000u8);
        let read_packet = ReadPacket::new(reader);

        let (_, (p, _)) = write_packet.join(read_packet).wait().unwrap();
        let decoded_packet = p.unwrap();

        assert!(decoded_packet.is_stream_packet());
        assert!(!decoded_packet.is_end_packet());
        assert!(decoded_packet.is_buffer_packet());
        assert_eq!(decoded_packet.id(), id);
        assert_eq!(decoded_packet.into_data(), expected_data);

        return true;
    }

    #[test]
    fn test_error_on_invalid_packet_type() {
        let (mut writer, reader) = ring_buffer(8);
        assert_eq!(writer.write(&[3u8]).unwrap(), 1); // header with invalid type flags
        let read_packet = ReadPacket::new(reader);
        let e = read_packet.wait().err().unwrap();
        assert_eq!(e.kind(), InvalidData);
    }

    #[test]
    fn zero_header() {
        let rng = StdGen::new(rand::thread_rng(), 20);
        let mut quickcheck = QuickCheck::new().gen(rng).tests(1000);
        quickcheck.quickcheck(test_zero_header as
                              fn(PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>)
                                 -> bool);
    }

    fn test_zero_header(write_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                        read_ops: PartialWithErrors<GenInterruptedWouldBlock>)
                        -> bool {
        let (writer, reader) = ring_buffer(64);
        let writer = PartialAsyncWrite::new(writer, write_ops);
        let reader = PartialAsyncRead::new(reader, read_ops);
        let write_zeros = WriteZeros::new(writer);
        let read_packet = ReadPacket::new(reader);

        let (_, (p, _)) = write_zeros.join(read_packet).wait().unwrap();
        assert!(p.is_none());

        return true;
    }

    #[test]
    fn codec_sink_stream() {
        let rng = StdGen::new(rand::thread_rng(), 2000);
        let mut quickcheck = QuickCheck::new().gen(rng).tests(200);
        quickcheck.quickcheck(test_codec_sink_stream as
                              fn(usize,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 Vec<u8>)
                                 -> bool);
    }

    fn test_codec_sink_stream(buf_size: usize,
                              write_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                              read_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                              data: Vec<u8>)
                              -> bool {
        let expected_data = data.clone();

        let (writer, reader) = ring_buffer(buf_size + 1);
        let writer = PartialAsyncWrite::new(writer, write_ops);
        let reader = PartialAsyncRead::new(reader, read_ops);

        let sink = PSCodecSink::new(writer);
        let stream = PSCodecStream::new(reader);

        let send = sink.send_all(iter_ok::<_, Error>((0..data.len()).map(|i| {
                                                                             (vec![data[i]],
                                                                              MetaData {
                                                                                  flags: 0,
                                                                                  id: i as PacketId,
                                                                              })
                                                                         })));

        let (received, _) = stream.collect().join(send).wait().unwrap();

        for (i, decoded_packet) in received.iter().enumerate() {
            if (i as PacketId) != decoded_packet.id() || decoded_packet.is_stream_packet() ||
               decoded_packet.is_end_packet() ||
               (!decoded_packet.is_buffer_packet() ||
                decoded_packet.data() != &vec![expected_data[i]]) {
                return false;
            }
        }

        return true;
    }
}
