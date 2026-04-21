use owo_colors::{OwoColorize, Style};
use std::fmt;
use supports_color::Stream;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

pub struct Styler {
    enabled: bool,
}

impl Styler {
    pub fn new(choice: ColorChoice) -> Self {
        let enabled = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                std::env::var_os("NO_COLOR").is_none()
                    && supports_color::on(Stream::Stdout).is_some()
            }
        };
        Self { enabled }
    }

    pub fn path<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().cyan())
            } else {
                None
            },
        }
    }

    pub fn dir_entry<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().blue().bold())
            } else {
                None
            },
        }
    }

    pub fn file_entry<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().white())
            } else {
                None
            },
        }
    }

    pub fn symlink_entry<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().cyan())
            } else {
                None
            },
        }
    }

    pub fn error<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().red().bold())
            } else {
                None
            },
        }
    }

    pub fn success<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().green())
            } else {
                None
            },
        }
    }

    pub fn header<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().bold().underline())
            } else {
                None
            },
        }
    }

    pub fn size<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().yellow())
            } else {
                None
            },
        }
    }

    pub fn warning<'a>(&self, s: &'a str) -> impl fmt::Display + 'a {
        ColoredStr {
            s,
            style: if self.enabled {
                Some(Style::new().yellow().bold())
            } else {
                None
            },
        }
    }
}

struct ColoredStr<'a> {
    s: &'a str,
    style: Option<Style>,
}

impl fmt::Display for ColoredStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.style {
            Some(style) => write!(f, "{}", self.s.style(*style)),
            None => write!(f, "{}", self.s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_produces_no_ansi() {
        let styler = Styler::new(ColorChoice::Never);
        let s = format!("{}", styler.error("ERROR"));
        assert!(
            !s.contains('\x1b'),
            "expected no ANSI escape codes, got: {s:?}"
        );
    }

    #[test]
    fn always_produces_ansi() {
        let styler = Styler::new(ColorChoice::Always);
        let s = format!("{}", styler.error("ERROR"));
        assert!(s.contains('\x1b'), "expected ANSI escape codes, got: {s:?}");
    }

    #[test]
    fn never_colored_str_display() {
        let styler = Styler::new(ColorChoice::Never);
        assert_eq!(format!("{}", styler.path("/foo")), "/foo");
    }
}
