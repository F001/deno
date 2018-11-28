// Copyright 2018 the Deno authors. All rights reserved. MIT license.
use libdeno;
use std::ffi::CStr;

// The version number is extracted from Cargo.toml. The expanded form is:
// pub const DENO: &'static str = "0.0.0";
include!(concat!(env!("OUT_DIR"), "/version_num.rs"));

pub fn v8() -> &'static str {
  let version = unsafe { libdeno::deno_v8_version() };
  let c_str = unsafe { CStr::from_ptr(version) };
  c_str.to_str().unwrap()
}
