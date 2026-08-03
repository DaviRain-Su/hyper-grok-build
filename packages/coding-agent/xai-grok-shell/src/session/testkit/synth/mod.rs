//! On-disk session synthesis. [`replay`] writes
//! `updates.jsonl`/`rewind_points.jsonl` envelopes directly for exact
//! ACU/rewind control; [`bench`] appends through the real storage adapter up to
//! a byte target for fork/copy benchmarks.

pub mod bench;
pub mod replay;

pub use bench::synthesize_to_target_bytes;
pub use replay::{
    SessionSpec, expected_replay_lines, locate_session_dir, prepare_session, sid,
    write_rewind_jsonl,
};
