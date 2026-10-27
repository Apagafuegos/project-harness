//! Host capability reporting uses the same probe as the process backend.
pub use harness_core::tools::sandbox::{
    DEFAULT_BWRAP, DEFAULT_SYSTEMD_RUN, Programs, host_report as report,
};
