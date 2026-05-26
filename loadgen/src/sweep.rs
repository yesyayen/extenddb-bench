//! RPS sweep CSV parsing.

use std::path::Path;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Sweep {
    pub steps: Vec<u64>,
}

impl Sweep {
    pub fn from_csv(s: &str) -> Result<Self> {
        let steps = s
            .split([',', '\n'])
            .map(str::trim)
            .filter(|t| !t.is_empty() && !t.starts_with('#'))
            .map(|t| t.parse::<u64>().with_context(|| format!("invalid RPS value: {t:?}")))
            .collect::<Result<Vec<_>>>()?;
        if steps.is_empty() {
            bail!("RPS sweep is empty");
        }
        if steps.iter().any(|&v| v == 0) {
            bail!("RPS values must be positive");
        }
        Ok(Self { steps })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading sweep file {}", path.display()))?;
        Self::from_csv(&contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_csv() {
        let s = Sweep::from_csv("1000,5000,25000").unwrap();
        assert_eq!(s.steps, vec![1000, 5000, 25000]);
    }

    #[test]
    fn ignores_comments_and_blanks() {
        let s = Sweep::from_csv("# header\n100\n\n  500 ,\n# trailing\n1000").unwrap();
        assert_eq!(s.steps, vec![100, 500, 1000]);
    }

    #[test]
    fn rejects_empty() {
        assert!(Sweep::from_csv("# nothing").is_err());
    }

    #[test]
    fn rejects_zero() {
        assert!(Sweep::from_csv("0,1000").is_err());
    }
}
