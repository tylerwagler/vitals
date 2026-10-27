//! One module per subsystem. Each has `static_()` for what the machine is and
//! `live(...)` for what it is doing; neither fails for a reading it cannot
//! take, which is null or absent, never 0.

pub mod cpu;
pub mod gpu;
pub mod host;
pub mod memory;
pub mod network;
pub mod pci;
pub mod processes;
pub mod sensors;
pub mod storage;
