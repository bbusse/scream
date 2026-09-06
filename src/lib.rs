// scream: screen streaming server
//
// main.rs is the binary: it parses the CLI, builds the GStreamer and Wayland
// machinery and wires these modules together. Everything that can be exercised
// without a compositor or an encoder lives here so it can be unit tested
//
// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 Björn Busse

pub mod dlna;
pub mod hls;
pub mod http;
pub mod layout;
pub mod metrics;
