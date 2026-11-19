//! Building PV names.
//!
//! `MIM:double:37` is thirteen characters long. Real facility names can be
//! thirty or forty characters, such as `SR:C14:BPM-2:POS:X`. At a million
//! channels, this difference is significant: name length affects search
//! payload sizes and, more importantly, the memory consumers use for
//! name-to-channel maps. Benchmarks using only short names therefore
//! underestimate an archiver's memory footprint.

/// A pattern for generating PV names.
///
/// Supported placeholders, optionally with a zero-padded width:
///
/// | Token | Meaning |
/// |---|---|
/// | `{i}` | Global channel index |
/// | `{ioc}` | Index of the virtual IOC serving the channel |
/// | `{t}` | Template name, e.g. `ntscalar_double` |
/// | `{n}` | Channel index within its IOC |
///
/// For example, `{i:06}` pads the global index to six digits.
///
/// The pattern must produce a unique name for every channel. Otherwise,
/// channels collide and some become unreachable. Uniqueness requires either
/// `{i}`, or the combination of `{ioc}` and `{n}`. The latter identifies
/// each channel just as reliably while producing names closer to those
/// used in real facilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTemplate {
    pattern: String,
}

/// Reasons a name pattern cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    /// The pattern does not uniquely identify each channel.
    NotUnique,
    /// The pattern contains an unrecognized `{...}` token.
    UnknownToken(String),
    /// The pattern contains an opening `{` without a matching `}`.
    Unterminated,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUnique => {
                f.write_str("pattern cannot name channels distinctly: use {i}, or {ioc} with {n}")
            }
            Self::UnknownToken(t) => {
                write!(f, "unknown token {{{t}}}; known tokens are i, ioc, t and n")
            }
            Self::Unterminated => f.write_str("unterminated { in pattern"),
        }
    }
}

impl std::error::Error for NameError {}

/// Values used to expand a name pattern.
#[derive(Debug, Clone, Copy)]
pub struct NameContext<'a> {
    pub index: u32,
    pub ioc: u16,
    pub index_in_ioc: u32,
    pub template: &'a str,
}

impl NameTemplate {
    /// Validates a pattern once, before it is used to generate channel names.
    ///
    /// Checks every token and ensures the pattern can uniquely identify
    /// each channel, avoiding repeated validation during large expansions.
    pub fn new(pattern: &str) -> Result<Self, NameError> {
        let (mut has_index, mut has_ioc, mut has_n) = (false, false, false);
        let bytes = pattern.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'{' {
                i += 1;
                continue;
            }
            let end = pattern[i..]
                .find('}')
                .map(|o| i + o)
                .ok_or(NameError::Unterminated)?;
            let token = &pattern[i + 1..end];
            let name = token.split(':').next().unwrap_or("");
            match name {
                "i" => has_index = true,
                "ioc" => has_ioc = true,
                "n" => has_n = true,
                "t" => {}
                other => return Err(NameError::UnknownToken(other.to_string())),
            }
            i = end + 1;
        }
        if !has_index && !(has_ioc && has_n) {
            return Err(NameError::NotUnique);
        }
        Ok(Self {
            pattern: pattern.to_string(),
        })
    }

    /// Returns the default naming pattern: short, clearly synthetic, and easy to recognize in logs.
    pub fn default_pattern(prefix: &str) -> Self {
        Self {
            pattern: format!("{prefix}:{{t}}:{{i}}"),
        }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Expands the pattern for a single channel.
    pub fn expand(&self, ctx: &NameContext<'_>) -> String {
        let mut out = String::with_capacity(self.pattern.len() + 16);
        let bytes = self.pattern.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'{' {
                out.push(bytes[i] as char);
                i += 1;
                continue;
            }
            let Some(end) = self.pattern[i..].find('}').map(|o| i + o) else {
                out.push('{');
                i += 1;
                continue;
            };
            let token = &self.pattern[i + 1..end];
            let (name, width) = match token.split_once(':') {
                None => (token, 0usize),
                Some((n, w)) => (n, w.parse().unwrap_or(0)),
            };
            match name {
                "i" => push_padded(&mut out, ctx.index as u64, width),
                "ioc" => push_padded(&mut out, ctx.ioc as u64, width),
                "n" => push_padded(&mut out, ctx.index_in_ioc as u64, width),
                "t" => out.push_str(ctx.template),
                _ => {}
            }
            i = end + 1;
        }
        out
    }
}

fn push_padded(out: &mut String, value: u64, width: usize) {
    let s = value.to_string();
    for _ in s.len()..width {
        out.push('0');
    }
    out.push_str(&s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> NameContext<'static> {
        NameContext {
            index: 37,
            ioc: 4,
            index_in_ioc: 7,
            template: "ntscalar_double",
        }
    }

    #[test]
    fn expands_every_token() {
        let t = NameTemplate::new("SR:C{ioc}:{t}-{n}:{i}").unwrap();
        assert_eq!(t.expand(&ctx()), "SR:C4:ntscalar_double-7:37");
    }

    #[test]
    fn pads_to_the_requested_width() {
        let t = NameTemplate::new("SR:C{ioc:02}:BPM-{n:03}:{i:06}").unwrap();
        assert_eq!(t.expand(&ctx()), "SR:C04:BPM-007:000037");
    }

    #[test]
    fn a_value_wider_than_its_padding_is_not_truncated() {
        // Padding sets a minimum width, not a maximum. Truncation could
        // cause distinct channel indices to produce duplicate names.
        let t = NameTemplate::new("{i:02}").unwrap();
        let c = NameContext {
            index: 123456,
            ..ctx()
        };
        assert_eq!(t.expand(&c), "123456");
    }

    #[test]
    fn the_default_is_short_and_obviously_synthetic() {
        let t = NameTemplate::default_pattern("MIM");
        assert_eq!(t.expand(&ctx()), "MIM:ntscalar_double:37");
    }

    #[test]
    fn a_pattern_that_cannot_distinguish_channels_is_refused() {
        // Patterns that generate duplicate names make some channels
        // unreachable and must therefore be rejected.
        assert_eq!(
            NameTemplate::new("SR:C{ioc}:FIXED"),
            Err(NameError::NotUnique)
        );
        assert_eq!(NameTemplate::new("FIXED"), Err(NameError::NotUnique));
        assert_eq!(
            NameTemplate::new("{t}:{n}"),
            Err(NameError::NotUnique),
            "index within an IOC is not unique on its own"
        );
    }

    #[test]
    fn ioc_and_index_within_it_are_enough() {
        // The pair provides the same uniqueness as the global index,
        // while producing names closer to those used in real facilities.
        let t = NameTemplate::new("SR:C{ioc:02}:BPM-{n:05}:POS").unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 0..20_000u32 {
            let c = NameContext {
                index: i,
                ioc: (i % 8) as u16,
                index_in_ioc: i / 8,
                template: "ntscalar_double",
            };
            assert!(seen.insert(t.expand(&c)), "duplicate at {i}");
        }
    }

    #[test]
    fn an_unknown_token_is_refused_rather_than_dropped() {
        assert_eq!(
            NameTemplate::new("{i}:{subsystem}"),
            Err(NameError::UnknownToken("subsystem".into()))
        );
    }

    #[test]
    fn an_unterminated_brace_is_refused() {
        assert_eq!(NameTemplate::new("{i}:{oops"), Err(NameError::Unterminated));
    }

    #[test]
    fn names_stay_unique_across_a_large_population() {
        let t = NameTemplate::new("SR:C{ioc:02}:DEV-{n:05}:{i:07}:VAL").unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 0..50_000u32 {
            let c = NameContext {
                index: i,
                ioc: (i % 20) as u16,
                index_in_ioc: i / 20,
                template: "ntscalar_double",
            };
            assert!(seen.insert(t.expand(&c)), "duplicate at {i}");
        }
    }

    #[test]
    fn a_realistic_pattern_is_meaningfully_longer_than_the_default() {
        // Benchmarks using only short synthetic names underestimate the
        // memory required for consumers to maintain name-to-channel maps.
        let short = NameTemplate::default_pattern("MIM").expand(&ctx());
        let real = NameTemplate::new("SR:C{ioc:02}:BPM-{n:03}:POS:X:{i:06}")
            .unwrap()
            .expand(&ctx());
        assert!(real.len() > short.len(), "{real} vs {short}");
    }
}
