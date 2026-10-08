//! Choosing the graceful timeout: the flag, the environment variable, or a
//! default for the container runtime scinit detects.
//!
//! No runtime tells the container its configured stop timeout, so a detected
//! runtime only selects that runtime's default. Each default is a little
//! below the runtime's own, so scinit's SIGKILL comes first and is logged.

use crate::Result;
use eyre::eyre;
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// Environment variable read when `--graceful-timeout-secs` isn't given
pub const GRACEFUL_TIMEOUT_VAR: &str = "SCINIT_GRACEFUL_TIMEOUT_SECS";

/// What runtime detection looks at: scinit's environment variables and
/// files. Separate from the child's [`crate::environment::Environment`].
pub trait HostEnv {
    fn var(&self, key: &str) -> Option<String>;
    fn file_exists(&self, path: &Path) -> bool;
}

/// The real process environment and filesystem
pub struct ProcessHostEnv;

impl HostEnv for ProcessHostEnv {
    fn var(&self, key: &str) -> Option<String> {
        // Lossy so a non-UTF-8 value is reported as invalid, not as unset
        std::env::var_os(key).map(|value| value.to_string_lossy().into_owned())
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// A container runtime scinit can recognise from inside the container
pub struct RuntimeProfile {
    pub name: &'static str,
    detect: fn(&dyn HostEnv) -> bool,
    pub default_graceful_timeout: Duration,
}

impl RuntimeProfile {
    pub fn detect(&self, env: &dyn HostEnv) -> bool {
        (self.detect)(env)
    }
}

/// Kubernetes sets the API server's address in every container. Its
/// default `terminationGracePeriodSeconds` is 30; 25 leaves time for scinit's
/// SIGKILL and exit before the kubelet's.
const KUBERNETES: RuntimeProfile = RuntimeProfile {
    name: "kubernetes",
    detect: |env| {
        env.var("KUBERNETES_SERVICE_HOST")
            .is_some_and(|v| !v.is_empty())
    },
    default_graceful_timeout: Duration::from_secs(25),
};

/// podman creates `/run/.containerenv` in its containers. `podman stop`
/// waits 10 seconds by default.
const PODMAN: RuntimeProfile = RuntimeProfile {
    name: "podman",
    detect: |env| env.file_exists(Path::new("/run/.containerenv")),
    default_graceful_timeout: Duration::from_secs(8),
};

/// Docker creates `/.dockerenv` in its containers. `docker stop` waits 10
/// seconds by default.
const DOCKER: RuntimeProfile = RuntimeProfile {
    name: "docker",
    detect: |env| env.file_exists(Path::new("/.dockerenv")),
    default_graceful_timeout: Duration::from_secs(8),
};

/// Profiles in detection order; the first match wins. Kubernetes comes
/// first, since a pod's container can also carry the marker file of the
/// runtime underneath.
const PROFILES: &[RuntimeProfile] = &[KUBERNETES, PODMAN, DOCKER];

/// Without a detected runtime: just under Docker's and podman's 10 seconds,
/// the shortest common stop timeout
pub const GENERIC_GRACEFUL_TIMEOUT: Duration = Duration::from_secs(8);

/// The graceful timeout and where its value came from
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GracefulTimeout {
    pub duration: Duration,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Flag,
    EnvVar,
    Detected(&'static str),
    Generic,
}

impl fmt::Display for GracefulTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s (", self.duration.as_secs())?;
        match self.source {
            Source::Flag => write!(f, "--graceful-timeout-secs")?,
            Source::EnvVar => write!(f, "{}", GRACEFUL_TIMEOUT_VAR)?,
            Source::Detected(name) => write!(f, "detected: {}", name)?,
            Source::Generic => write!(f, "no runtime detected")?,
        }
        write!(f, ")")
    }
}

/// Resolves the graceful timeout: `flag_secs`, then
/// `SCINIT_GRACEFUL_TIMEOUT_SECS`, then the detected runtime's default, then
/// [`GENERIC_GRACEFUL_TIMEOUT`]. An invalid environment variable is an error.
pub fn resolve(flag_secs: Option<u64>, env: &dyn HostEnv) -> Result<GracefulTimeout> {
    if let Some(secs) = flag_secs {
        return Ok(GracefulTimeout {
            duration: Duration::from_secs(secs),
            source: Source::Flag,
        });
    }
    if let Some(value) = env.var(GRACEFUL_TIMEOUT_VAR) {
        let secs: u64 = value.trim().parse().map_err(|_| {
            eyre!(
                "Invalid {} '{}': expected a whole number of seconds",
                GRACEFUL_TIMEOUT_VAR,
                value
            )
        })?;
        return Ok(GracefulTimeout {
            duration: Duration::from_secs(secs),
            source: Source::EnvVar,
        });
    }
    Ok(match detect_runtime(env) {
        Some(profile) => GracefulTimeout {
            duration: profile.default_graceful_timeout,
            source: Source::Detected(profile.name),
        },
        None => GracefulTimeout {
            duration: GENERIC_GRACEFUL_TIMEOUT,
            source: Source::Generic,
        },
    })
}

/// The first profile that matches `env`
fn detect_runtime(env: &dyn HostEnv) -> Option<&'static RuntimeProfile> {
    PROFILES.iter().find(|profile| profile.detect(env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeHostEnv {
        vars: HashMap<String, String>,
        files: Vec<PathBuf>,
    }

    impl FakeHostEnv {
        fn var(mut self, key: &str, value: &str) -> Self {
            self.vars.insert(key.to_string(), value.to_string());
            self
        }

        fn file(mut self, path: &str) -> Self {
            self.files.push(PathBuf::from(path));
            self
        }
    }

    impl HostEnv for FakeHostEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }

        fn file_exists(&self, path: &Path) -> bool {
            self.files.iter().any(|file| file == path)
        }
    }

    fn detected(env: &FakeHostEnv) -> Option<&'static str> {
        detect_runtime(env).map(|profile| profile.name)
    }

    #[test]
    fn test_detects_each_runtime() {
        let env = FakeHostEnv::default().var("KUBERNETES_SERVICE_HOST", "10.0.0.1");
        assert_eq!(detected(&env), Some("kubernetes"));
        let env = FakeHostEnv::default().file("/run/.containerenv");
        assert_eq!(detected(&env), Some("podman"));
        let env = FakeHostEnv::default().file("/.dockerenv");
        assert_eq!(detected(&env), Some("docker"));
        assert_eq!(detected(&FakeHostEnv::default()), None);
    }

    #[test]
    fn test_kubernetes_wins_over_marker_files() {
        let env = FakeHostEnv::default()
            .var("KUBERNETES_SERVICE_HOST", "10.0.0.1")
            .file("/run/.containerenv")
            .file("/.dockerenv");
        assert_eq!(detected(&env), Some("kubernetes"));
    }

    #[test]
    fn test_empty_kubernetes_host_is_not_kubernetes() {
        let env = FakeHostEnv::default().var("KUBERNETES_SERVICE_HOST", "");
        assert_eq!(detected(&env), None);
    }

    #[test]
    fn test_profile_defaults() {
        let timeout = |env: FakeHostEnv| resolve(None, &env).unwrap();
        assert_eq!(
            timeout(FakeHostEnv::default().var("KUBERNETES_SERVICE_HOST", "10.0.0.1")),
            GracefulTimeout {
                duration: Duration::from_secs(25),
                source: Source::Detected("kubernetes"),
            }
        );
        assert_eq!(
            timeout(FakeHostEnv::default().file("/run/.containerenv")).duration,
            Duration::from_secs(8)
        );
        assert_eq!(
            timeout(FakeHostEnv::default().file("/.dockerenv")).duration,
            Duration::from_secs(8)
        );
        assert_eq!(
            timeout(FakeHostEnv::default()),
            GracefulTimeout {
                duration: Duration::from_secs(8),
                source: Source::Generic,
            }
        );
    }

    #[test]
    fn test_env_var_wins_over_detection() {
        let env = FakeHostEnv::default()
            .var("KUBERNETES_SERVICE_HOST", "10.0.0.1")
            .var(GRACEFUL_TIMEOUT_VAR, "55");
        assert_eq!(
            resolve(None, &env).unwrap(),
            GracefulTimeout {
                duration: Duration::from_secs(55),
                source: Source::EnvVar,
            }
        );
    }

    #[test]
    fn test_flag_wins_over_env_var() {
        let env = FakeHostEnv::default()
            .var(GRACEFUL_TIMEOUT_VAR, "not-a-number")
            .file("/.dockerenv");
        assert_eq!(
            resolve(Some(3), &env).unwrap(),
            GracefulTimeout {
                duration: Duration::from_secs(3),
                source: Source::Flag,
            }
        );
    }

    #[test]
    fn test_invalid_env_var_is_an_error() {
        for value in ["", "abc", "-1", "1.5", "10s"] {
            let env = FakeHostEnv::default().var(GRACEFUL_TIMEOUT_VAR, value);
            let err = resolve(None, &env).unwrap_err().to_string();
            assert!(
                err.contains(GRACEFUL_TIMEOUT_VAR),
                "{:?}: unexpected error {}",
                value,
                err
            );
        }
    }

    #[test]
    fn test_display_names_the_source() {
        let shown = |source| {
            GracefulTimeout {
                duration: Duration::from_secs(25),
                source,
            }
            .to_string()
        };
        assert_eq!(
            shown(Source::Detected("kubernetes")),
            "25s (detected: kubernetes)"
        );
        assert_eq!(shown(Source::Flag), "25s (--graceful-timeout-secs)");
        assert_eq!(shown(Source::EnvVar), "25s (SCINIT_GRACEFUL_TIMEOUT_SECS)");
        assert_eq!(shown(Source::Generic), "25s (no runtime detected)");
    }
}
