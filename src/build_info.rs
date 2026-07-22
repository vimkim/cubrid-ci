/// Git commit from which this executable was built.
pub const GIT_COMMIT: &str = env!("CUBRID_CI_BUILD_GIT_SHA");

/// User-facing version containing both the Cargo package version and source commit.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CUBRID_CI_BUILD_GIT_SHA"),
    ")"
);
