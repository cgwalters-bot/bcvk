//! Detect running inside a toolbox or distrobox container, to help explain
//! podman failures there.
//!
//! bcvk is designed to be installed on the host, as a peer of podman: to
//! launch a VM it asks podman to bind-mount its own binary and the host's
//! `/usr` (for QEMU) into a new container. From a toolbox or distrobox,
//! `podman` is often forwarded to the host (via a `flatpak-spawn --host`
//! wrapper or the podman socket), so those paths are resolved on the host
//! rather than in the container bcvk runs in. If e.g. bcvk itself only exists
//! inside the container, podman then fails with an obscure
//! `statfs /usr/bin/bcvk: no such file or directory`.
//!
//! Running podman inside the toolbox is legitimate too, and a forwarding
//! wrapper can't be reliably told apart from it, so nothing here refuses to
//! run; it only adds guidance when podman fails in the characteristic way.
//!
//! See <https://github.com/bootc-dev/bcvk/issues/5>.

use std::fmt;
use std::io::ErrorKind;

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre::Report;
use tracing::{debug, info};

/// Created by toolbox in its containers.
const TOOLBOXENV_PATH: &str = "/run/.toolboxenv";
/// Created by podman in every container it runs.
const CONTAINERENV_PATH: &str = "/run/.containerenv";
/// Set by toolbox in its containers.
const TOOLBOX_PATH_ENV: &str = "TOOLBOX_PATH";
/// Set by distrobox to the container name.
const DISTROBOX_CONTAINER_ID_ENV: &str = "CONTAINER_ID";
/// Where both toolbox and distrobox mount the host's root filesystem.
const HOST_ROOT: &str = "/run/host";
/// Documentation for running bcvk from a toolbox or distrobox.
const DOCS_URL: &str =
    "https://github.com/bootc-dev/bcvk/blob/main/docs/src/installation.md#toolbox-and-distrobox";
/// How podman reports a bind mount source that doesn't exist, e.g.
/// `Error: statfs /usr/bin/bcvk: no such file or directory`.
const PODMAN_STATFS_PREFIX: &str = "statfs ";
const ENOENT_MESSAGE: &str = "no such file or directory";

/// A development container that typically forwards podman to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DevContainer {
    Toolbox,
    Distrobox,
}

impl fmt::Display for DevContainer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DevContainer::Toolbox => "toolbox",
            DevContainer::Distrobox => "distrobox",
        })
    }
}

/// The inputs to detection, gathered separately so the logic can be tested.
#[derive(Debug, Default)]
struct Environment<'a> {
    /// Whether `/run/.toolboxenv` exists.
    toolboxenv: bool,
    /// The contents of `/run/.containerenv`, if it exists.
    containerenv: Option<&'a str>,
    /// Whether `TOOLBOX_PATH` is set.
    toolbox_path: bool,
    /// Whether `CONTAINER_ID` is set.
    container_id: bool,
}

/// Parse the `key="value"` lines of `/run/.containerenv`.
fn parse_containerenv(contents: &str) -> impl Iterator<Item = (&str, &str)> {
    contents.lines().filter_map(|line| {
        let (k, v) = line.split_once('=')?;
        Some((k.trim(), v.trim().trim_matches('"')))
    })
}

fn detect(env: &Environment) -> Option<DevContainer> {
    if env.toolboxenv || env.toolbox_path {
        return Some(DevContainer::Toolbox);
    }
    // Everything below only applies to podman containers; this also keeps a
    // stray CONTAINER_ID in a non-container environment from matching.
    let containerenv = env.containerenv?;
    if env.container_id {
        return Some(DevContainer::Distrobox);
    }
    // Fall back to hints in the container and image names. Distrobox
    // containers are often created from toolbox images, so it takes priority.
    let hints: Vec<&str> = parse_containerenv(containerenv)
        .filter(|(k, _)| matches!(*k, "name" | "image"))
        .map(|(_, v)| v)
        .collect();
    if hints.iter().any(|v| v.contains("distrobox")) {
        Some(DevContainer::Distrobox)
    } else if hints.iter().any(|v| v.contains("toolbox")) {
        Some(DevContainer::Toolbox)
    } else {
        None
    }
}

/// Detect whether we are running inside a toolbox or distrobox container.
///
/// This is only used for hints, so I/O errors are logged and treated as "not
/// detected" rather than failing.
pub(crate) fn detect_current() -> Option<DevContainer> {
    let toolboxenv = Utf8Path::new(TOOLBOXENV_PATH)
        .try_exists()
        .unwrap_or_else(|e| {
            debug!("Checking for {TOOLBOXENV_PATH}: {e}");
            false
        });
    let containerenv = match std::fs::read_to_string(CONTAINERENV_PATH) {
        Ok(s) => Some(s),
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                debug!("Reading {CONTAINERENV_PATH}: {e}");
            }
            None
        }
    };
    let env = Environment {
        toolboxenv,
        containerenv: containerenv.as_deref(),
        toolbox_path: std::env::var_os(TOOLBOX_PATH_ENV).is_some(),
        container_id: std::env::var_os(DISTROBOX_CONTAINER_ID_ENV).is_some(),
    };
    let r = detect(&env);
    debug!("Development container detection: {r:?} from {env:?}");
    r
}

/// The path at which `path` in the host's root filesystem is visible from
/// inside a toolbox or distrobox container.
fn host_path(path: &Utf8Path) -> Utf8PathBuf {
    Utf8Path::new(HOST_ROOT).join(path.strip_prefix("/").unwrap_or(path))
}

/// Log a hint up front if we are inside a toolbox or distrobox and our own
/// binary is not visible at the same path on the host, which is where a
/// host-forwarded podman will look for it.
pub(crate) fn log_self_exe_hint(self_exe: &Utf8Path) {
    let Some(kind) = detect_current() else {
        return;
    };
    let on_host = host_path(self_exe);
    match on_host.try_exists() {
        Ok(true) => debug!("Running in a {kind}; {self_exe} also exists on the host"),
        Ok(false) if Utf8Path::new(HOST_ROOT).is_dir() => info!(
            "Running in a {kind}, and {self_exe} is not present on the host; \
             this will fail if podman is forwarded to the host. See {DOCS_URL}"
        ),
        Ok(false) => debug!("Running in a {kind} without {HOST_ROOT}; can't check the host"),
        Err(e) => debug!("Checking for {on_host}: {e}"),
    }
}

/// Whether podman's stderr says a bind mount source doesn't exist.
fn is_missing_bind_source(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains(PODMAN_STATFS_PREFIX) && l.contains(ENOENT_MESSAGE))
}

/// Wrap `err` with guidance if `kind` is set and `stderr` shows a missing
/// bind mount source.
fn with_hint_for(kind: Option<DevContainer>, err: Report, stderr: &str) -> Report {
    let Some(kind) = kind.filter(|_| is_missing_bind_source(stderr)) else {
        return err;
    };
    err.wrap_err(format!(
        "bcvk is running inside a {kind} container, and podman could not find a path \
         it was asked to bind-mount. If podman is forwarded to the host, the bcvk binary \
         and any bind mount sources must exist at the same path on the host, not just in \
         this container.\n\
         \n\
         Consider installing bcvk on the host alongside podman and QEMU (e.g. \
         `sudo dnf install bcvk`, or add it to your image on image-based systems) and \
         running it there; from a {kind} you can use `flatpak-spawn --host bcvk ...`.\n\
         See {DOCS_URL}"
    ))
}

/// Add toolbox guidance to the error for a failed podman invocation, if we
/// are in a toolbox or distrobox and podman failed to find a bind mount
/// source.
pub(crate) fn with_podman_failure_hint(err: Report, stderr: &str) -> Report {
    // Cheap check first, so unrelated failures skip detection entirely.
    if !is_missing_bind_source(stderr) {
        return err;
    }
    with_hint_for(detect_current(), err, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLBOX_CONTAINERENV: &str = r#"engine="podman-5.6.1"
name="fedora-toolbox-43"
id="0123abcd"
image="registry.fedoraproject.org/fedora-toolbox:43"
imageid="4567ef"
rootless=1
"#;
    const DISTROBOX_CONTAINERENV: &str = r#"engine="podman-5.6.1"
name="my-distrobox"
id="0123abcd"
image="quay.io/toolbx/arch-toolbox:latest"
imageid="4567ef"
rootless=1
"#;
    const PLAIN_CONTAINERENV: &str = r#"engine="podman-5.6.1"
name="devcontainer"
id="0123abcd"
image="ghcr.io/bootc-dev/devenv-debian:latest"
imageid="4567ef"
rootless=1
"#;

    #[test]
    fn test_detect() {
        let cases = [
            ("host", Environment::default(), None),
            (
                "toolboxenv",
                Environment {
                    toolboxenv: true,
                    containerenv: Some(TOOLBOX_CONTAINERENV),
                    ..Default::default()
                },
                Some(DevContainer::Toolbox),
            ),
            (
                "TOOLBOX_PATH",
                Environment {
                    toolbox_path: true,
                    ..Default::default()
                },
                Some(DevContainer::Toolbox),
            ),
            (
                "toolbox image name only",
                Environment {
                    containerenv: Some(TOOLBOX_CONTAINERENV),
                    ..Default::default()
                },
                Some(DevContainer::Toolbox),
            ),
            (
                "distrobox CONTAINER_ID",
                Environment {
                    containerenv: Some(DISTROBOX_CONTAINERENV),
                    container_id: true,
                    ..Default::default()
                },
                Some(DevContainer::Distrobox),
            ),
            (
                "distrobox name wins over toolbox image",
                Environment {
                    containerenv: Some(DISTROBOX_CONTAINERENV),
                    ..Default::default()
                },
                Some(DevContainer::Distrobox),
            ),
            (
                "CONTAINER_ID outside a container",
                Environment {
                    container_id: true,
                    ..Default::default()
                },
                None,
            ),
            (
                "other podman container",
                Environment {
                    containerenv: Some(PLAIN_CONTAINERENV),
                    ..Default::default()
                },
                None,
            ),
            (
                "empty containerenv",
                Environment {
                    containerenv: Some(""),
                    ..Default::default()
                },
                None,
            ),
        ];
        for (desc, env, expected) in cases {
            assert_eq!(detect(&env), expected, "{desc}");
        }
    }

    #[test]
    fn test_with_hint_for() {
        const STATFS: &str = "Error: statfs /usr/bin/bcvk: no such file or directory\n";
        const OTHER: &str = "Error: short-name resolution enforced\n";
        let cases = [
            (Some(DevContainer::Toolbox), STATFS, true),
            (Some(DevContainer::Distrobox), STATFS, true),
            (None, STATFS, false),
            (Some(DevContainer::Toolbox), OTHER, false),
            (Some(DevContainer::Toolbox), "", false),
        ];
        for (kind, stderr, hinted) in cases {
            let msg = format!("Podman command failed: {stderr}");
            let r = with_hint_for(kind, color_eyre::eyre::eyre!(msg.clone()), stderr);
            let chain: Vec<String> = r.chain().map(|e| e.to_string()).collect();
            if hinted {
                let kind = kind.unwrap();
                assert_eq!(chain.len(), 2, "{kind} {stderr:?}");
                assert!(
                    chain[0].contains(&format!("inside a {kind} container")),
                    "{chain:?}"
                );
                assert!(chain[0].contains(DOCS_URL), "{chain:?}");
                // The original podman error is kept as the cause.
                assert_eq!(chain[1], msg);
            } else {
                assert_eq!(chain, [msg], "{kind:?} {stderr:?}");
            }
        }
    }

    #[test]
    fn test_host_path() {
        for (input, expected) in [
            ("/usr/bin/bcvk", "/run/host/usr/bin/bcvk"),
            (
                "/var/home/user/.cargo/bin/bcvk",
                "/run/host/var/home/user/.cargo/bin/bcvk",
            ),
        ] {
            assert_eq!(host_path(Utf8Path::new(input)), expected);
        }
    }
}
