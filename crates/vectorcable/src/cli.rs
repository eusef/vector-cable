//! Command line: `vectorcable [up|status|doctor|setup <app>|soak|install-driver|uninstall-driver]`
//! with the flags from brief section 8. Hand-parsed: no dependency needed.

use crate::theme::ThemeName;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Bare `vectorcable`: the TUI (or `--plain`).
    Up,
    Status,
    Doctor,
    Setup(String),
    Soak,
    InstallDriver,
    UninstallDriver,
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub command: Command,
    pub listen: bool,
    pub bridge: Option<String>,
    pub plain: bool,
    /// Hidden: `--audio null` selects the null adapter.
    pub audio: Option<String>,
    pub theme: Option<ThemeName>,
    pub reduce_motion: bool,
    pub minutes: Option<u64>,
    /// The local port for the rig control relay, over the config file's.
    pub rig_port: Option<u16>,
}

pub const USAGE: &str = "\
Usage: vectorcable [command] [options]

Commands
  (none)            Open the terminal UI
  up                Connect and open the terminal UI
  status            Print the bridge version, protocol, mode and loss
  doctor            Run the checks and print a verdict for each
  setup <app>       Print the settings for WSJT-X, JS8Call or FLDIGI
  soak [--minutes N]  Transmit silence with the gate off and report loss
  install-driver    Install the virtual audio driver
  uninstall-driver  Remove the virtual audio driver

Options
  --listen          Listen Only: never take the transmit slot
  --bridge HOST     Bridge name or address (default: find it)
  --plain           Line output instead of the terminal UI
  --theme night-ops Start in Night Ops
  --reduce-motion   Meters update once a second
  --rig-port N      Rig control relay port on 127.0.0.1 (default 4532)
  --help, --version";

pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        command: Command::Up,
        listen: false,
        bridge: None,
        plain: false,
        audio: None,
        theme: None,
        reduce_motion: false,
        minutes: None,
        rig_port: None,
    };
    let mut iter = args.iter().peekable();
    let mut command_seen = false;
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--listen" => out.listen = true,
            "--plain" => out.plain = true,
            "--reduce-motion" => out.reduce_motion = true,
            "--help" | "-h" => out.command = Command::Help,
            "--version" | "-V" => out.command = Command::Version,
            "--bridge" => {
                out.bridge = Some(iter.next().ok_or("--bridge needs a host")?.to_string())
            }
            "--audio" => out.audio = Some(iter.next().ok_or("--audio needs a name")?.to_string()),
            "--theme" => {
                let v = iter.next().ok_or("--theme needs standard or night-ops")?;
                out.theme = Some(ThemeName::parse(v).ok_or_else(|| format!("unknown theme {v}"))?);
            }
            "--minutes" => {
                let v = iter.next().ok_or("--minutes needs a number")?;
                out.minutes = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--minutes: {v} is not a number"))?,
                );
            }
            "--rig-port" => {
                out.rig_port = Some(rig_port(iter.next().ok_or("--rig-port needs a number")?)?)
            }
            s if s.starts_with("--rig-port=") => out.rig_port = Some(rig_port(&s[11..])?),
            s if s.starts_with("--bridge=") => out.bridge = Some(s[9..].to_string()),
            s if s.starts_with("--audio=") => out.audio = Some(s[8..].to_string()),
            s if s.starts_with("--theme=") => {
                out.theme = Some(
                    ThemeName::parse(&s[8..])
                        .ok_or_else(|| format!("unknown theme {}", &s[8..]))?,
                )
            }
            s if s.starts_with("--minutes=") => {
                out.minutes = Some(
                    s[10..]
                        .parse::<u64>()
                        .map_err(|_| format!("--minutes: {} is not a number", &s[10..]))?,
                )
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if !command_seen => {
                command_seen = true;
                out.command = match s {
                    "up" => Command::Up,
                    "status" => Command::Status,
                    "doctor" => Command::Doctor,
                    "soak" => Command::Soak,
                    "install-driver" => Command::InstallDriver,
                    "uninstall-driver" => Command::UninstallDriver,
                    "help" => Command::Help,
                    "version" => Command::Version,
                    "setup" => {
                        let app = iter
                            .next()
                            .ok_or("setup needs an app: wsjt-x, js8call or fldigi")?;
                        Command::Setup(app.to_string())
                    }
                    other => return Err(format!("unknown command {other}")),
                };
            }
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    Ok(out)
}

fn rig_port(v: &str) -> Result<u16, String> {
    v.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| format!("--rig-port: {v} is not a port (1 to 65535)"))
}

/// `--plain` is implied when stdout is not a terminal or `TERM` is `dumb`.
pub fn plain_implied(stdout_is_tty: bool, term: Option<&str>) -> bool {
    !stdout_is_tty || term == Some("dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        let v: Vec<String> = s.split_whitespace().map(str::to_string).collect();
        parse(&v)
    }

    #[test]
    fn subcommands_and_flags_parse() {
        assert_eq!(p("").unwrap().command, Command::Up);
        let a = p("up --listen --bridge vectorbridge.local --plain --audio null").unwrap();
        assert_eq!(a.command, Command::Up);
        assert!(a.listen && a.plain);
        assert_eq!(a.bridge.as_deref(), Some("vectorbridge.local"));
        assert_eq!(a.audio.as_deref(), Some("null"));
        assert_eq!(p("status").unwrap().command, Command::Status);
        assert_eq!(p("doctor").unwrap().command, Command::Doctor);
        assert_eq!(
            p("setup wsjt-x").unwrap().command,
            Command::Setup("wsjt-x".into())
        );
        let s = p("soak --minutes 3").unwrap();
        assert_eq!(s.command, Command::Soak);
        assert_eq!(s.minutes, Some(3));
        assert_eq!(p("install-driver").unwrap().command, Command::InstallDriver);
        assert_eq!(
            p("uninstall-driver").unwrap().command,
            Command::UninstallDriver
        );
        assert_eq!(
            p("--theme night-ops --reduce-motion").unwrap().theme,
            Some(ThemeName::NightOps)
        );
        assert!(!p("--theme=standard").unwrap().reduce_motion);
        assert!(p("setup").is_err());
        assert!(p("--bogus").is_err());
        assert!(p("fly").is_err());
        assert!(p("--minutes x").is_err());
    }

    #[test]
    fn the_rig_port_takes_a_port_in_either_spelling() {
        assert_eq!(p("").unwrap().rig_port, None);
        assert_eq!(p("--rig-port 4533").unwrap().rig_port, Some(4533));
        assert_eq!(
            p("setup wsjt-x --rig-port=4600").unwrap().rig_port,
            Some(4600)
        );
        assert!(p("--rig-port 0").is_err());
        assert!(p("--rig-port 70000").is_err());
        assert!(p("--rig-port").is_err());
    }

    #[test]
    fn plain_is_implied_without_a_tty_or_with_term_dumb() {
        assert!(plain_implied(false, Some("xterm-256color")));
        assert!(plain_implied(true, Some("dumb")));
        assert!(!plain_implied(true, Some("xterm-256color")));
    }
}
