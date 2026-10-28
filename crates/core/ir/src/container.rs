//! Typed options for a container scope and its sidecar services, and the
//! lowering of the engine-flag text a CI format hands us (GitHub's
//! `container.options` and `services.<id>.options`) into them.
//!
//! The set is the one the compatibility corpus and the acceptance battery
//! use: env, user, DNS, added capabilities, privilege, platform, and for a
//! service an entrypoint and a health check. A flag outside the set is a
//! lowering error naming the flag, so a workflow never runs with an option
//! silently dropped — that failure belongs at parse time, where the author
//! can act on it, not inside an executor's acquire.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Options a job's container accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ContainerOptions {
    /// `-e KEY=VALUE`; applied after the scope env, so a flag wins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env:        Vec<(SmolStr, SmolStr)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user:       Option<SmolStr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns:        Vec<SmolStr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cap_add:    Vec<SmolStr>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform:   Option<SmolStr>,
}

/// Options a service container accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ServiceOptions {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env:        Vec<(SmolStr, SmolStr)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user:       Option<SmolStr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<SmolStr>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns:        Vec<SmolStr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cap_add:    Vec<SmolStr>,
    /// Run the service privileged: what a Docker-in-Docker service needs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    /// When set, the service must report healthy before the scope's first
    /// step; without one it is started and not waited on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health:     Option<HealthCheck>,
}

/// A service's health check, in the shape Docker's own takes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// The check command, run by the container's default shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd:             Option<SmolStr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms:     Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms:      Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries:         Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period_ms: Option<u64>,
}

/// Why a flag did not lower: the flag as written and the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionError {
    pub flag: String,
    pub why:  String,
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "container option `{}` is not supported: {}",
            self.flag, self.why
        )
    }
}

impl Error for OptionError {}

/// Lowers a job container's raw flags.
pub fn parse_container_options(flags: &[SmolStr]) -> Result<ContainerOptions, OptionError> {
    let mut options = ContainerOptions::default();
    let mut tokens = Tokens::new(flags);
    while let Some((flag, inline)) = tokens.next_flag() {
        match flag.as_str() {
            "-e" | "--env" => {
                let pair = tokens.value(&flag, inline)?;
                options.env.push(env_pair(&flag, &pair)?);
            }
            "-u" | "--user" => options.user = Some(tokens.value(&flag, inline)?),
            "--dns" => options.dns.push(tokens.value(&flag, inline)?),
            "--cap-add" => options.cap_add.push(tokens.value(&flag, inline)?),
            "--platform" => options.platform = Some(tokens.value(&flag, inline)?),
            "--privileged" => options.privileged = true,
            "--entrypoint"
            | "--health-cmd"
            | "--health-interval"
            | "--health-timeout"
            | "--health-retries"
            | "--health-start-period" => {
                return Err(unsupported(
                    &flag,
                    "it applies to a service container, not the job container",
                ));
            }
            _ => return Err(unsupported(&flag, "no typed mapping exists for it")),
        }
    }
    Ok(options)
}

/// Lowers a service container's raw flags.
pub fn parse_service_options(flags: &[SmolStr]) -> Result<ServiceOptions, OptionError> {
    let mut options = ServiceOptions::default();
    let mut tokens = Tokens::new(flags);
    while let Some((flag, inline)) = tokens.next_flag() {
        match flag.as_str() {
            "-e" | "--env" => {
                let pair = tokens.value(&flag, inline)?;
                options.env.push(env_pair(&flag, &pair)?);
            }
            "-u" | "--user" => options.user = Some(tokens.value(&flag, inline)?),
            "--entrypoint" => options.entrypoint = Some(vec![tokens.value(&flag, inline)?]),
            "--dns" => options.dns.push(tokens.value(&flag, inline)?),
            "--cap-add" => options.cap_add.push(tokens.value(&flag, inline)?),
            "--health-cmd" => health(&mut options).cmd = Some(tokens.value(&flag, inline)?),
            "--health-interval" => {
                health(&mut options).interval_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-timeout" => {
                health(&mut options).timeout_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-start-period" => {
                health(&mut options).start_period_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-retries" => {
                let raw = tokens.value(&flag, inline)?;
                let retries = raw
                    .parse::<u64>()
                    .map_err(|_| unsupported(&flag, &format!("`{raw}` is not a whole number")))?;
                health(&mut options).retries = Some(retries);
            }
            "--privileged" => options.privileged = true,
            "--platform" => {
                return Err(unsupported(
                    &flag,
                    "it applies to the job container, not a service",
                ));
            }
            _ => return Err(unsupported(&flag, "no typed mapping exists for it")),
        }
    }
    Ok(options)
}

fn health(options: &mut ServiceOptions) -> &mut HealthCheck {
    options.health.get_or_insert_with(HealthCheck::default)
}

/// Splits `KEY=VALUE`. A bare `KEY` would mean "inherit from the runner's
/// environment" on GitHub, which no sandbox has; it is rejected.
fn env_pair(flag: &str, pair: &str) -> Result<(SmolStr, SmolStr), OptionError> {
    match pair.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((SmolStr::new(key), SmolStr::new(value))),
        _ => Err(unsupported(
            flag,
            &format!(
                "`{pair}` must be `KEY=VALUE`; a bare name has no runner environment to inherit"
            ),
        )),
    }
}

/// A Docker duration: a number with an `ms`, `s`, `m`, or `h` suffix (a bare
/// number is seconds, as `docker run` reads it).
fn duration_ms(flag: &str, raw: &str) -> Result<u64, OptionError> {
    let (digits, unit) = raw
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((raw, "s"), |at| raw.split_at(at));
    let value: f64 = digits
        .parse()
        .map_err(|_| unsupported(flag, &format!("`{raw}` is not a duration")))?;
    let scale = match unit {
        "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        _ => return Err(unsupported(flag, &format!("`{raw}` has an unknown unit"))),
    };
    let millis = Duration::from_secs_f64(value * scale / 1_000.0).as_millis();
    u64::try_from(millis).map_err(|_| unsupported(flag, &format!("`{raw}` is out of range")))
}

fn unsupported(flag: &str, why: &str) -> OptionError {
    OptionError {
        flag: flag.to_owned(),
        why:  why.to_owned(),
    }
}

/// A cursor over shell-split flags that reads `--flag value` and
/// `--flag=value` alike.
struct Tokens<'a> {
    flags: &'a [SmolStr],
    at:    usize,
}

impl<'a> Tokens<'a> {
    fn new(flags: &'a [SmolStr]) -> Self {
        Self { flags, at: 0 }
    }

    /// The next flag and, for `--flag=value`, its inline value.
    fn next_flag(&mut self) -> Option<(String, Option<SmolStr>)> {
        let token = self.flags.get(self.at)?;
        self.at += 1;
        if token.starts_with("--")
            && let Some((flag, value)) = token.split_once('=')
        {
            return Some((flag.to_owned(), Some(SmolStr::new(value))));
        }
        Some((token.to_string(), None))
    }

    /// The flag's value: inline, or the next token.
    fn value(&mut self, flag: &str, inline: Option<SmolStr>) -> Result<SmolStr, OptionError> {
        if let Some(value) = inline {
            return Ok(value);
        }
        let value = self
            .flags
            .get(self.at)
            .ok_or_else(|| unsupported(flag, "it needs a value"))?;
        self.at += 1;
        Ok(value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(text: &str) -> Vec<SmolStr> {
        text.split_whitespace().map(SmolStr::new).collect()
    }

    #[test]
    fn container_flags_lower_in_both_spellings() {
        let options = parse_container_options(&flags(
            "-e A=1 --env=B=2 --user root --dns 127.0.0.1 --cap-add=NET_ADMIN --privileged --platform linux/amd64",
        ))
        .expect("parses");
        assert_eq!(options.env, vec![
            ("A".into(), "1".into()),
            ("B".into(), "2".into())
        ]);
        assert_eq!(options.user.as_deref(), Some("root"));
        assert_eq!(options.dns, vec![SmolStr::new("127.0.0.1")]);
        assert_eq!(options.cap_add, vec![SmolStr::new("NET_ADMIN")]);
        assert!(options.privileged);
        assert_eq!(options.platform.as_deref(), Some("linux/amd64"));
    }

    #[test]
    fn a_service_health_check_lowers_to_milliseconds() {
        let options = parse_service_options(&flags(
            "--health-cmd redis-cli --health-interval 1s --health-timeout 500ms --health-retries 30 --entrypoint /bin/sh",
        ))
        .expect("parses");
        let health = options.health.expect("health");
        assert_eq!(health.cmd.as_deref(), Some("redis-cli"));
        assert_eq!(health.interval_ms, Some(1_000));
        assert_eq!(health.timeout_ms, Some(500));
        assert_eq!(health.retries, Some(30));
        assert_eq!(options.entrypoint, Some(vec![SmolStr::new("/bin/sh")]));
    }

    #[test]
    fn unknown_and_misplaced_flags_are_rejected_by_name() {
        let error = parse_container_options(&flags("--shm-size 1g")).expect_err("rejects");
        assert_eq!(error.flag, "--shm-size");
        let error = parse_container_options(&flags("--entrypoint sh")).expect_err("rejects");
        assert!(error.why.contains("service container"), "{error}");
        let error = parse_container_options(&flags("-e BARE")).expect_err("rejects");
        assert!(error.why.contains("KEY=VALUE"), "{error}");
        let error = parse_service_options(&flags("--platform linux/arm64")).expect_err("rejects");
        assert!(error.why.contains("job container"), "{error}");
        // A Docker-in-Docker service needs `--privileged`, so a service takes it.
        let options = parse_service_options(&flags("--privileged")).expect("parses");
        assert!(options.privileged);
    }

    #[test]
    fn options_round_trip_through_json_with_unset_fields_absent() {
        let options = parse_container_options(&flags("--privileged")).expect("parses");
        let json = serde_json::to_value(&options).expect("encodes");
        assert_eq!(json, serde_json::json!({ "privileged": true }));
        let back: ContainerOptions = serde_json::from_value(json).expect("decodes");
        assert_eq!(back, options);
    }
}
