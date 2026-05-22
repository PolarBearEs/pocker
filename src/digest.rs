use std::fmt;
use std::io::Read;
use std::path::Path;

use sha2::{Sha256, Sha384, Sha512};

use crate::error::{DockerPullError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParsedDigest<'a> {
    pub algorithm: DigestAlgorithm,
    pub value: &'a str,
}

impl DigestAlgorithm {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "sha256" => Ok(Self::Sha256),
            "sha384" => Ok(Self::Sha384),
            "sha512" => Ok(Self::Sha512),
            other => Err(DockerPullError::UnsupportedDigestAlgorithm(
                other.to_string(),
            )),
        }
    }

    fn hex_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha384 => 96,
            Self::Sha512 => 128,
        }
    }

    pub fn digest_bytes(self, bytes: &[u8]) -> String {
        match self {
            Self::Sha256 => format!("{}:{}", self, hash_bytes::<Sha256>(bytes)),
            Self::Sha384 => format!("{}:{}", self, hash_bytes::<Sha384>(bytes)),
            Self::Sha512 => format!("{}:{}", self, hash_bytes::<Sha512>(bytes)),
        }
    }

    fn digest_reader(self, mut reader: impl Read) -> Result<String> {
        let mut buffer = [0_u8; 64 * 1024];
        match self {
            Self::Sha256 => hash_reader::<Sha256>(&mut reader, &mut buffer)
                .map(|digest| format!("{}:{digest}", self)),
            Self::Sha384 => hash_reader::<Sha384>(&mut reader, &mut buffer)
                .map(|digest| format!("{}:{digest}", self)),
            Self::Sha512 => hash_reader::<Sha512>(&mut reader, &mut buffer)
                .map(|digest| format!("{}:{digest}", self)),
        }
    }
}

impl fmt::Display for DigestAlgorithm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256 => formatter.write_str("sha256"),
            Self::Sha384 => formatter.write_str("sha384"),
            Self::Sha512 => formatter.write_str("sha512"),
        }
    }
}

pub(crate) fn parse_digest(digest: &str) -> Result<ParsedDigest<'_>> {
    let (algorithm, value) = digest.split_once(':').ok_or_else(|| {
        DockerPullError::InvalidInput(format!("invalid digest format `{digest}`"))
    })?;
    let algorithm = DigestAlgorithm::parse(algorithm)?;
    if value.len() != algorithm.hex_len()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(DockerPullError::InvalidInput(format!(
            "invalid {algorithm} digest value `{value}`"
        )));
    }
    Ok(ParsedDigest { algorithm, value })
}

pub(crate) fn digest_hex(digest: &str) -> Result<&str> {
    Ok(parse_digest(digest)?.value)
}

pub(crate) fn digest_bytes_for_digest(digest: &str, bytes: &[u8]) -> Result<String> {
    Ok(parse_digest(digest)?.algorithm.digest_bytes(bytes))
}

pub(crate) fn digest_file_for_digest(digest: &str, path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    parse_digest(digest)?.algorithm.digest_reader(file)
}

pub(crate) fn canonical_digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hash_bytes::<Sha256>(bytes)
}

fn hash_bytes<D: sha2::Digest>(bytes: &[u8]) -> String {
    let mut hasher = D::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn hash_reader<D: sha2::Digest>(reader: &mut impl Read, buffer: &mut [u8]) -> Result<String> {
    let mut hasher = D::new();
    loop {
        let read = reader.read(buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::{DigestAlgorithm, digest_bytes_for_digest, digest_hex, parse_digest};
    use crate::error::DockerPullError;

    #[test]
    fn parse_digest_accepts_supported_algorithms() {
        for (algorithm, hex_len) in [("sha256", 64), ("sha384", 96), ("sha512", 128)] {
            let digest = format!("{algorithm}:{}", "a".repeat(hex_len));
            let parsed = parse_digest(&digest).expect("supported digest should parse");

            assert_eq!(parsed.algorithm.to_string(), algorithm);
            assert_eq!(parsed.value.len(), hex_len);
        }
    }

    #[test]
    fn parse_digest_rejects_unknown_algorithm() {
        let error = parse_digest("sha224:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect_err("unsupported digest algorithm should fail");

        assert!(
            matches!(error, DockerPullError::UnsupportedDigestAlgorithm(algorithm) if algorithm == "sha224")
        );
    }

    #[test]
    fn parse_digest_rejects_invalid_hex_values() {
        for digest in [
            "sha256:abc",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        ] {
            let error = parse_digest(digest).expect_err("invalid digest value should fail");

            assert!(matches!(error, DockerPullError::InvalidInput(_)));
        }
    }

    #[test]
    fn digest_hex_returns_validated_value() {
        let value = "a".repeat(64);
        let digest = format!("sha256:{value}");

        assert_eq!(digest_hex(&digest).expect("digest should parse"), value);
        assert!(digest_hex("sha256:abc").is_err());
    }

    #[test]
    fn digest_bytes_uses_requested_algorithm() {
        let bytes = b"pocker";
        let sha384_template = format!("sha384:{}", "0".repeat(96));
        let actual =
            digest_bytes_for_digest(&sha384_template, bytes).expect("supported digest should hash");

        assert_eq!(actual, DigestAlgorithm::Sha384.digest_bytes(bytes));
        assert!(actual.starts_with("sha384:"));
    }
}
