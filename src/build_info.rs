/// Git commit from which this executable was built.
pub const GIT_COMMIT: &str = env!("CUBRID_CI_BUILD_GIT_SHA");

/// Cargo build class (`debug` or `release`) used for this executable.
pub const BUILD_PROFILE: &str = env!("CUBRID_CI_BUILD_PROFILE");

/// User-facing version containing the package version, source commit, and build class.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CUBRID_CI_BUILD_GIT_SHA"),
    ", ",
    env!("CUBRID_CI_BUILD_PROFILE"),
    ")"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_contains_commit_and_build_profile() {
        assert!(VERSION.contains(GIT_COMMIT));
        assert!(VERSION.contains(BUILD_PROFILE));
        assert!(matches!(BUILD_PROFILE, "debug" | "release"));
    }
}
