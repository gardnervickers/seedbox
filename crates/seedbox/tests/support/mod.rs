//! Helpers shared by this executable's integration tests. Test roots declare
//! `pub mod support;` so helpers one binary does not use are not dead code.
pub mod application;
pub mod derived;
pub mod etcd_project;
pub mod failure_capture;
pub mod json_diff;
