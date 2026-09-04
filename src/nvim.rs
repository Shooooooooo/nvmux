//! Finding and version-checking the `nvim` binary.
//!
//! Checked up front and reported plainly, because every failure mode here
//! otherwise surfaces much later as an unexplained connection error.

use std::process::Command;

use crate::error::NvimError;

/// The minimum Neovim nvmux supports.
///
/// 0.11 specifically because that is where `:detach` and `:connect` landed,
/// which is what makes a session survive its UI going away.
pub const MIN_VERSION: &str = "0.11";
const MIN: (u64, u64) = (0, 11);

/// A parsed `nvim --version` banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `dev`, `dev-1511`, and so on. Present on nightlies.
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

/// Parse the first line of `nvim --version`.
///
/// Has to survive rather more shapes than it looks like:
///
/// ```text
/// NVIM v0.11.4
/// NVIM v0.13.0-dev-1511+g5209695703
/// NVIM v0.12.0-dev
/// NVIM v0.12.0-dev-145+g0f9113907-dirty     (built from a dirty tree)
/// NVIM v0.11.4+ubuntu1                      (distro patch suffix)
/// ```
///
/// Distro packagers can override `NVIM_VERSION_MEDIUM` outright, so this never
/// keys on a byte offset and never anchors the tail.
pub fn parse_version(banner: &str) -> Option<Version> {
    let line = banner.lines().next()?.trim();
    let token = line
        .split_whitespace()
        .find(|t| {
            let t = t.strip_prefix('v').unwrap_or(t);
            t.chars().next().is_some_and(|c| c.is_ascii_digit())
        })?
        .strip_prefix("NVIM")
        .unwrap_or_else(|| {
            line.split_whitespace()
                .find(|t| {
                    let t = t.strip_prefix('v').unwrap_or(t);
                    t.chars().next().is_some_and(|c| c.is_ascii_digit())
                })
                .unwrap_or("")
        });

    let token = token.trim().strip_prefix('v').unwrap_or(token.trim());
    // Build metadata after '+' is never semantically meaningful to us.
    let (core, _build) = token.split_once('+').unwrap_or((token, ""));
    let (numbers, prerelease) = match core.split_once('-') {
        Some((n, p)) => (n, Some(p.to_string())),
        None => (core, None),
    };

    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A two-component version is legal; treat the missing patch as 0.
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

/// Check the local `nvim`, the one used as the `--remote-ui` client.
pub fn check_local() -> Result<Version, NvimError> {
    let out = Command::new("nvim")
        .arg("--version")
        .output()
        .map_err(|_| NvimError::NotFound {
            where_: "local".into(),
            min: MIN_VERSION,
        })?;
    let banner = String::from_utf8_lossy(&out.stdout);
    let version = parse_version(&banner).ok_or_else(|| NvimError::UnparsableVersion {
        where_: "local".into(),
        raw: banner.lines().next().unwrap_or("").to_string(),
    })?;
    if !version.is_supported() {
        return Err(NvimError::TooOld {
            where_: "local".into(),
            found: version.to_string(),
            min: MIN_VERSION,
        });
    }
    Ok(version)
}

/// The minimum `ssh` nvmux supports.
///
/// 6.7 is where unix-domain socket forwarding (`-L <local_sock>:<remote_sock>`)
/// was added, which is the entire remote transport.
pub const MIN_SSH_VERSION: &str = "6.7";

/// Parse the version out of `ssh -V` output, e.g. `OpenSSH_9.6p1 Ubuntu-3...`.
pub fn parse_ssh_version(banner: &str) -> Option<(u64, u64)> {
    let token = banner.split_whitespace().next()?;
    let rest = token.strip_prefix("OpenSSH_")?;
    let numeric: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
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
    fn parses_ssh_banners() {
        assert_eq!(
            parse_ssh_version("OpenSSH_9.6p1 Ubuntu-3ubuntu13.19, OpenSSL 3.0.13"),
            Some((9, 6))
        );
        assert_eq!(
            parse_ssh_version("OpenSSH_9.0p1, LibreSSL 3.3.6"),
            Some((9, 0))
        );
        assert_eq!(parse_ssh_version("OpenSSH_6.7p1"), Some((6, 7)));
        assert_eq!(parse_ssh_version("something else"), None);
    }

    /// Runs against whatever nvim is actually installed, when there is one.
    #[test]
    fn agrees_with_the_real_binary() {
        let Ok(out) = Command::new("nvim").arg("--version").output() else {
            return; // No nvim here; the unit cases above still cover the parser.
        };
        let banner = String::from_utf8_lossy(&out.stdout);
        let parsed = parse_version(&banner).expect("real nvim banner must parse");
        assert!(
            parsed.major > 0 || parsed.minor > 0,
            "parsed {parsed} from {banner:?}"
        );
    }
}
