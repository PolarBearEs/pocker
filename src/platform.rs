use serde::{Deserialize, Serialize};

use crate::error::{DockerPullError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl Platform {
    pub fn host() -> Self {
        Self {
            os: normalize_os(std::env::consts::OS).to_string(),
            architecture: normalize_architecture(std::env::consts::ARCH).to_string(),
            variant: None,
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        let mut parts = value.split('/');
        let os = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| DockerPullError::InvalidInput("platform os is required".into()))?;
        let architecture = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| {
                DockerPullError::InvalidInput("platform architecture is required".into())
            })?;
        let variant = parts
            .next()
            .map(|part| {
                if part.is_empty() {
                    return Err(DockerPullError::InvalidInput(
                        "platform variant cannot be empty".into(),
                    ));
                }
                Ok(part.to_string())
            })
            .transpose()?;
        if parts.next().is_some() {
            return Err(DockerPullError::InvalidInput(
                "platform must use os/arch[/variant] format".into(),
            ));
        }
        Ok(Self {
            os: normalize_os(os).to_string(),
            architecture: normalize_architecture(architecture).to_string(),
            variant,
        })
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.os == other.os
            && self.architecture == other.architecture
            && (self.variant.is_none() || self.variant == other.variant)
    }

    pub fn as_string(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}/{variant}", self.os, self.architecture),
            None => format!("{}/{}", self.os, self.architecture),
        }
    }
}

fn normalize_os(value: &str) -> &str {
    match value {
        "macos" => "darwin",
        other => other,
    }
}

fn normalize_architecture(value: &str) -> &str {
    match value {
        "x86_64" => "amd64",
        "x86" | "i386" | "i586" | "i686" => "386",
        "aarch64" => "arm64",
        "armv7l" | "armv7" => "arm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    #[test]
    fn parse_platform() {
        let platform = Platform::parse("linux/arm64/v8").expect("platform should parse");
        assert_eq!(platform.os, "linux");
        assert_eq!(platform.architecture, "arm64");
        assert_eq!(platform.variant.as_deref(), Some("v8"));
    }

    #[test]
    fn matches_without_variant_requirement() {
        let requested = Platform::parse("linux/arm64").expect("platform should parse");
        let candidate = Platform::parse("linux/arm64/v8").expect("platform should parse");
        assert!(requested.matches(&candidate));
    }

    #[test]
    fn normalizes_common_architecture_aliases() {
        for (input, expected) in [
            ("linux/x86_64", "amd64"),
            ("linux/x86", "386"),
            ("linux/i386", "386"),
            ("linux/i586", "386"),
            ("linux/i686", "386"),
            ("linux/aarch64", "arm64"),
            ("linux/armv7l", "arm"),
            ("linux/armv7", "arm"),
        ] {
            let platform = Platform::parse(input).expect("platform should parse");
            assert_eq!(platform.architecture, expected, "{input} should normalize");
        }
    }

    #[test]
    fn normalizes_macos_to_oci_darwin() {
        let platform = Platform::parse("macos/aarch64").expect("platform should parse");

        assert_eq!(platform.as_string(), "darwin/arm64");
    }

    #[test]
    fn rejects_platforms_with_too_many_segments() {
        let error = Platform::parse("linux/arm64/v8/extra")
            .expect_err("platform with extra segments should be rejected");
        assert_eq!(
            error.to_string(),
            "platform must use os/arch[/variant] format"
        );
    }

    #[test]
    fn rejects_empty_platform_variant() {
        let error = Platform::parse("linux/amd64/")
            .expect_err("platform with empty variant should be rejected");
        assert_eq!(error.to_string(), "platform variant cannot be empty");
    }
}
