//! Command line with the syntax and messages of Go's `flag` package:
//! `-config <path>` (default `config.yaml`), `-version`, `-h`/`-help`; `-flag=value` and
//! `--flag` accepted; parsing stops at the first non-flag argument or `--`.

/// What to do after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Run { config: String },
    Version,
}

/// A parse outcome that ends the program: text for stderr and exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct Exit {
    pub stderr: String,
    pub code: i32,
}

pub fn usage(program: &str) -> String {
    format!(
        "Usage of {program}:\n  -config string\n    \tFilepath to config.yaml (default \"config.yaml\")\n  -version\n    \tShow version\n"
    )
}

/// Go `strconv.ParseBool`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Parses `args` (without the program name).
pub fn parse(program: &str, args: &[String]) -> Result<Action, Exit> {
    let fail = |msg: String| Exit {
        stderr: format!("{msg}\n{}", usage(program)),
        code: 2,
    };
    let mut config = "config.yaml".to_string();
    let mut version = false;
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        if s.len() < 2 || !s.starts_with('-') {
            break;
        }
        let minuses = if s.as_bytes()[1] == b'-' {
            if s.len() == 2 {
                break;
            }
            2
        } else {
            1
        };
        let mut name = &s[minuses..];
        if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
            return Err(fail(format!("bad flag syntax: {s}")));
        }
        i += 1;
        let mut value = None;
        if let Some(eq) = name[1..].find('=') {
            value = Some(name[eq + 2..].to_string());
            name = &name[..eq + 1];
        }
        match name {
            "version" => match value {
                None => version = true,
                Some(v) => match parse_bool(&v) {
                    Some(b) => version = b,
                    None => {
                        return Err(fail(format!(
                            "invalid boolean value {v:?} for -version: parse error"
                        )));
                    }
                },
            },
            "config" => {
                let v = match value {
                    Some(v) => v,
                    None if i < args.len() => {
                        i += 1;
                        args[i - 1].clone()
                    }
                    None => return Err(fail("flag needs an argument: -config".to_string())),
                };
                config = v;
            }
            "h" | "help" => {
                return Err(Exit {
                    stderr: usage(program),
                    code: 0,
                });
            }
            other => return Err(fail(format!("flag provided but not defined: -{other}"))),
        }
    }
    if version {
        Ok(Action::Version)
    } else {
        Ok(Action::Run { config })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Action, Exit> {
        parse(
            "minijob",
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn flags_like_go() {
        assert_eq!(
            p(&[]),
            Ok(Action::Run {
                config: "config.yaml".into()
            })
        );
        assert_eq!(
            p(&["-config", "a.yaml"]),
            Ok(Action::Run {
                config: "a.yaml".into()
            })
        );
        assert_eq!(
            p(&["--config=b.yaml"]),
            Ok(Action::Run {
                config: "b.yaml".into()
            })
        );
        assert_eq!(
            p(&["-config", "a", "-config", "b"]),
            Ok(Action::Run { config: "b".into() })
        );
        assert_eq!(p(&["-version"]), Ok(Action::Version));
        assert_eq!(p(&["-config", "x", "-version=true"]), Ok(Action::Version));
        assert_eq!(
            p(&["-version=0"]),
            Ok(Action::Run {
                config: "config.yaml".into()
            })
        );
        assert_eq!(
            p(&["extra", "-version"]),
            Ok(Action::Run {
                config: "config.yaml".into()
            })
        );
        assert_eq!(
            p(&["--", "-version"]),
            Ok(Action::Run {
                config: "config.yaml".into()
            })
        );
        assert_eq!(p(&["-h"]).unwrap_err().code, 0);
        let e = p(&["-nope"]).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(
            e.stderr
                .starts_with("flag provided but not defined: -nope\nUsage of minijob:\n")
        );
        assert_eq!(
            p(&["-config"]).unwrap_err().stderr.lines().next(),
            Some("flag needs an argument: -config")
        );
        assert_eq!(
            p(&["-version=maybe"]).unwrap_err().stderr.lines().next(),
            Some("invalid boolean value \"maybe\" for -version: parse error")
        );
        assert_eq!(
            p(&["---x"]).unwrap_err().stderr.lines().next(),
            Some("bad flag syntax: ---x")
        );
    }
}
