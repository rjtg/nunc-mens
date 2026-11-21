#[cfg(not(unix))]
compile_error!("nunc requires a Unix filesystem and POSIX FIFOs");

mod chunk;
pub mod cli;
mod documents;
mod duplicates;
pub mod entries;
pub mod fifo;
mod fs;
mod git;
mod index;
mod ledger;
mod model;
mod protocol;
pub mod repo;
mod search;
mod vectors;
mod watch;
