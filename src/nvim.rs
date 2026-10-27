//! Finding and version-checking `nvim` up front; otherwise failures surface
//! later as unexplained connection errors.

use std::process::Command;

use crate::error::NvimError;

/// Minimum supported Neovim: 0.11 added `:detach`, which lets a session
/// outlive its UI. Must match `MIN` (a test checks).
pub const MIN_VERSION: &str = "0.11";
const MIN: (u64, u64) = (0, 11);

/// From an `nvim --version` banner or [`crate::rpc::Client::api_info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `dev`, `dev-1511`, etc. on nightlies.
    pub prerelease: Option<String>,
}

impl Version {
    pub fn is_supported(&self) -> bool {
        (self.major, self.minor) >= MIN
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.prerelease {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

/// Parse the first line of `nvim --version`. Shapes seen:
///
/// ```text
/// NVIM v0.11.4
/// NVIM v0.13.0-dev-1511+g5209695703
/// NVIM v0.12.0-dev
/// NVIM v0.12.0-dev-145+g0f9113907-dirty     (built from a dirty tree)
/// NVIM v0.11.4+ubuntu1                      (distro patch suffix)
/// ```
///
/// Packagers can override `NVIM_VERSION_MEDIUM`, so this never relies on byte
/// offsets or anchors the tail.
pub fn parse_version(banner: &str) -> Option<Version> {
    let line = banner.lines().next()?.trim();
    // First word that looks like `v0.11.4` or `0.11.4`.
    let token = line.split_whitespace().find(|t| {
        let t = t.strip_prefix('v').unwrap_or(t);
        t.chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;

    let token = token.strip_prefix('v').unwrap_or(token);
    // Build metadata after '+' is ignored.
    let (core, _build) = token.split_once('+').unwrap_or((token, ""));
    let (numbers, prerelease) = match core.split_once('-') {
        Some((n, p)) => (n, Some(p.to_string())),
        None => (core, None),
    };

    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // Two components are legal; patch defaults to 0.
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }

    Some(Version {
        major,
        minor,
        patch,
        prerelease,
    })
}

/// The local `nvim`'s version, asked once; `None` if it could not be.
pub fn local_version() -> Option<&'static Version> {
    static LOCAL: std::sync::OnceLock<Option<Version>> = std::sync::OnceLock::new();
    LOCAL.get_or_init(|| check_local().ok()).as_ref()
}

/// Check the local `nvim`, used for local sessions and as the `--remote-ui`
/// client.
pub fn check_local() -> Result<Version, NvimError> {
    let out = Command::new("nvim")
        .arg("--version")
        .output()
        .map_err(|_| NvimError::NotFound {
            where_: "local".into(),
            min: MIN_VERSION,
        })?;
    check_banner("local", &String::from_utf8_lossy(&out.stdout))
}

/// Check an `nvim --version` banner (local or remote) parses and is new
/// enough. `where_` names the machine in the error.
pub fn check_banner(where_: &str, banner: &str) -> Result<Version, NvimError> {
    let version = parse_version(banner).ok_or_else(|| NvimError::UnparsableVersion {
        where_: where_.into(),
        raw: banner.lines().next().unwrap_or("").to_string(),
    })?;
    if !version.is_supported() {
        return Err(NvimError::TooOld {
            where_: where_.into(),
            found: version.to_string(),
            min: MIN_VERSION,
        });
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64, patch: u64, pre: Option<&str>) -> Version {
        Version {
            major,
            minor,
            patch,
            prerelease: pre.map(String::from),
        }
    }

    #[test]
    fn parses_every_banner_shape_in_the_wild() {
        let cases = [
            ("NVIM v0.11.4", v(0, 11, 4, None)),
            (
                "NVIM v0.13.0-dev-1511+g5209695703",
                v(0, 13, 0, Some("dev-1511")),
            ),
            ("NVIM v0.12.0-dev", v(0, 12, 0, Some("dev"))),
            (
                "NVIM v0.12.0-dev-145+g0f9113907-dirty",
                v(0, 12, 0, Some("dev-145")),
            ),
            (
                "NVIM v0.12.0-dev-0f9113907",
                v(0, 12, 0, Some("dev-0f9113907")),
            ),
            ("NVIM v0.11.4+ubuntu1", v(0, 11, 4, None)),
            ("NVIM v0.10.2\nBuild type: Release", v(0, 10, 2, None)),
            ("NVIM v1.0", v(1, 0, 0, None)),
        ];
        for (banner, want) in cases {
            assert_eq!(
                parse_version(banner).as_ref(),
                Some(&want),
                "for {banner:?}"
            );
        }
    }

    #[test]
    fn rejects_nonsense_rather_than_guessing() {
        for banner in ["", "not a version", "NVIM", "NVIM vx.y.z", "NVIM v1.2.3.4"] {
            assert_eq!(parse_version(banner), None, "should not parse {banner:?}");
        }
    }

    #[test]
    fn the_version_gate_is_the_documented_one() {
        assert!(!v(0, 9, 5, None).is_supported());
        assert!(!v(0, 10, 4, None).is_supported());
        assert!(
            v(0, 11, 0, None).is_supported(),
            "0.11 is where :detach landed"
        );
        assert!(
            v(0, 13, 0, Some("dev")).is_supported(),
            "nightlies must pass"
        );
        assert!(v(1, 0, 0, None).is_supported());
    }

    #[test]
    fn display_round_trips() {
        assert_eq!(v(0, 11, 4, None).to_string(), "0.11.4");
        assert_eq!(v(0, 13, 0, Some("dev-1511")).to_string(), "0.13.0-dev-1511");
    }

    #[test]
    fn the_printed_minimum_is_the_enforced_one() {
        assert_eq!(MIN_VERSION, format!("{}.{}", MIN.0, MIN.1));
    }

    #[test]
    fn the_banner_gate_refuses_old_and_unreadable_banners_by_name() {
        assert_eq!(
            check_banner("myhost", "NVIM v0.11.4\nBuild type: Release").expect("new enough"),
            v(0, 11, 4, None)
        );
        match check_banner("myhost", "NVIM v0.9.5") {
            Err(NvimError::TooOld { where_, found, min }) => {
                assert_eq!(where_, "myhost");
                assert_eq!(found, "0.9.5");
                assert_eq!(min, MIN_VERSION);
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
        match check_banner("myhost", "garbage\nmore") {
            Err(NvimError::UnparsableVersion { where_, raw }) => {
                assert_eq!(where_, "myhost");
                assert_eq!(raw, "garbage", "the first line, which is the version line");
            }
            other => panic!("expected UnparsableVersion, got {other:?}"),
        }
    }

    #[test]
    fn agrees_with_the_real_binary() {
        let Ok(out) = Command::new("nvim").arg("--version").output() else {
            // Skip without nvim, unless `NVMUX_TEST_REQUIRE` demands it (CI).
            let strict = std::env::var("NVMUX_TEST_REQUIRE")
                .is_ok_and(|v| v.split(',').any(|w| w.trim() == "nvim"));
            assert!(
                !strict,
                "NVMUX_TEST_REQUIRE names nvim but it is not on $PATH"
            );
            return;
        };
        let banner = String::from_utf8_lossy(&out.stdout);
        let parsed = parse_version(&banner).expect("real nvim banner must parse");
        assert!(
            parsed.major > 0 || parsed.minor > 0,
            "parsed {parsed} from {banner:?}"
        );
    }
}
