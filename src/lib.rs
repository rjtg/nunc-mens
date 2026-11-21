#[cfg(not(unix))]
compile_error!("nunc requires a Unix filesystem and POSIX FIFOs");

pub mod cli;
pub mod entries;
mod fs;
pub mod repo;
