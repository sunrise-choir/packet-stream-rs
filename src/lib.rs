#![warn(missing_docs)]

extern crate futures;
extern crate tokio_io;
extern crate atm_io_utils;
extern crate void;
extern crate bytes;
extern crate multi_producer_sink;
extern crate multi_consumer_stream;

#[cfg(test)]
extern crate partial_io;
#[cfg(test)]
extern crate quickcheck;
#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate rand;

mod raw;
mod codec;
mod codec2; // TODO make thiss the codec implementation
pub mod ps;
