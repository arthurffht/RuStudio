//! Compiled DXBC shader bytecodes emitted by `build.rs`.

#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/shaders/mod.rs"));
