//! Fields needed for usable narinfo and closure traversal, following Nix 2.34's
//! `src/libstore/{nar-info,path,content-address}.cc` and
//! `src/libutil/{hash,compression,signature/local-keys}.cc`.
//! Experimental hash/content-address features are not assumed to be enabled.
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{collections::BTreeMap, path::Path};

const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

fn store_name(name: &str) -> bool {
    let Some((hash, name)) = name.split_once('-') else {
        return false;
    };
    hash.len() == 32
        && hash.bytes().all(|b| NIX32.contains(&b))
        && !name.is_empty()
        && name.len() <= 211
        && !matches!(name.split('-').next(), Some("." | ".."))
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(&b))
}

fn hash(value: &str) -> bool {
    let (algorithm, encoded, sri) = match value.split_once(':') {
        Some((algorithm, encoded)) => (algorithm, encoded, false),
        None => match value.split_once('-') {
            Some((algorithm, encoded)) => (algorithm, encoded, true),
            None => return false,
        },
    };
    let bytes: usize = match algorithm {
        "md5" => 16,
        "sha1" => 20,
        "sha256" => 32,
        "sha512" => 64,
        _ => return false,
    };
    if !sri && encoded.len() == bytes * 2 {
        return encoded.bytes().all(|b| b.is_ascii_hexdigit());
    }
    if !sri && encoded.len() == (bytes * 8).div_ceil(5) {
        let unused = bytes * 8 % 5;
        return encoded.bytes().all(|b| NIX32.contains(&b))
            && (unused == 0
                || NIX32
                    .iter()
                    .position(|b| Some(b) == encoded.as_bytes().first())
                    .is_some_and(|first| first < (1 << unused)));
    }
    encoded.len() == bytes.div_ceil(3) * 4
        && STANDARD
            .decode(encoded)
            .is_ok_and(|decoded| decoded.len() == bytes)
}

fn compression(value: &str) -> bool {
    // Nix's direct decoders plus libarchive's named filters (3.8.2,
    // libarchive/archive_write_add_filter_by_name.c).
    matches!(
        value,
        "" | "none"
            | "identity"
            | "br"
            | "b64encode"
            | "bzip2"
            | "compress"
            | "grzip"
            | "gzip"
            | "lrzip"
            | "lz4"
            | "lzip"
            | "lzma"
            | "lzop"
            | "uuencode"
            | "xz"
            | "zstd"
    )
}

fn signature(value: &str) -> bool {
    value.split_once(':').is_some_and(|(name, encoded)| {
        !name.is_empty()
            && STANDARD
                .decode(encoded)
                .is_ok_and(|bytes| bytes.len() == 64)
    })
}

fn content_address(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let hash_value = value
        .strip_prefix("text:")
        .or_else(|| value.strip_prefix("fixed:r:"))
        .or_else(|| value.strip_prefix("fixed:"));
    hash_value.is_some_and(|value| value.contains(':') && hash(value))
}

pub(super) fn references(body: &str, expected: &str) -> Result<Vec<String>, &'static str> {
    if body.is_empty() || !body.ends_with('\n') {
        return Err("empty or truncated narinfo");
    }
    let mut fields = BTreeMap::new();
    for line in body.split_terminator('\n') {
        let (name, value) = line.split_once(": ").ok_or("invalid narinfo field")?;
        // Signatures may occur repeatedly for distinct cache signing keys.
        if name == "Sig" && !signature(value) {
            return Err("invalid Sig");
        }
        if matches!(
            name,
            "StorePath"
                | "URL"
                | "NarHash"
                | "NarSize"
                | "References"
                | "FileHash"
                | "FileSize"
                | "Deriver"
                | "Compression"
                | "CA"
        ) && fields.insert(name, value).is_some()
        {
            return Err("duplicate narinfo field");
        }
    }
    let path = fields.get("StorePath").ok_or("StorePath missing")?;
    if *path != expected {
        return Err("StorePath does not match requested path");
    }
    let path = Path::new(expected);
    if !path.is_absolute()
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(store_name)
    {
        return Err("invalid StorePath");
    }
    if fields.get("URL").is_none_or(|url| url.is_empty()) {
        return Err("URL missing");
    }
    if fields
        .get("Compression")
        .is_some_and(|value| !compression(value))
    {
        return Err("invalid Compression");
    }
    if fields
        .get("CA")
        .is_some_and(|value| !content_address(value))
    {
        return Err("invalid CA");
    }
    if !fields.get("NarHash").is_some_and(|value| hash(value)) {
        return Err("invalid or missing NarHash");
    }
    if !fields
        .get("NarSize")
        .is_some_and(|size| size.parse::<u64>().is_ok_and(|size| size > 0))
    {
        return Err("invalid or missing NarSize");
    }
    if fields.get("FileHash").is_some_and(|value| !hash(value)) {
        return Err("invalid FileHash");
    }
    if fields
        .get("FileSize")
        .is_some_and(|size| size.parse::<u64>().is_err())
    {
        return Err("invalid FileSize");
    }
    if fields
        .get("Deriver")
        .is_some_and(|name| *name != "unknown-deriver" && !store_name(name))
    {
        return Err("invalid Deriver");
    }
    let store = path.parent().ok_or("StorePath has no store directory")?;
    fields
        .get("References")
        .unwrap_or(&"")
        .split(' ')
        .filter(|name| !name.is_empty())
        .map(|name| {
            if !store_name(name) {
                return Err("invalid References store path");
            }
            Ok(store.join(name).to_string_lossy().into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const PATH: &str = "/nix/store/00000000000000000000000000000000-app";
    fn leaf() -> String {
        format!(
            "StorePath: {PATH}\nURL: nar/app.nar\nNarHash: sha256:{}\nNarSize: 1\n",
            "0".repeat(52)
        )
    }

    #[test]
    fn valid_leaf_allows_empty_or_absent_references() {
        assert_eq!(references(&leaf(), PATH).unwrap(), Vec::<String>::new());
        assert_eq!(
            references(&(leaf() + "References: \n"), PATH).unwrap(),
            Vec::<String>::new()
        );
        assert!(store_name("00000000000000000000000000000000-.hidden"));
        assert!(!store_name("00000000000000000000000000000000-..-escape"));
    }

    #[test]
    fn malformed_fields_and_references_are_rejected() {
        let valid = leaf();
        for body in [
            valid.trim_end().into(),
            valid.replace("NarSize: 1", "NarSize: 0"),
            valid.replace("NarSize: 1", "NarSize: bad"),
            valid.replace("StorePath: ", "StorePath:"),
            valid.replace("URL: nar/app.nar\n", ""),
            valid.replace(&"0".repeat(52), "not-a-hash"),
            valid.clone() + "References: ../escape\n",
            valid.clone() + "References: invalid-reference\n",
            valid.clone() + "References: \nReferences: \n",
            valid.clone() + "FileSize: nope\n",
            valid.clone() + "FileHash: invalid\n",
            valid.clone() + "StorePath: /nix/store/00000000000000000000000000000000-app\n",
        ] {
            assert!(references(&body, PATH).is_err(), "{body:?}");
        }
    }

    #[test]
    fn hash_encodings_match_nix_formats() {
        for bytes in [16, 20, 32, 64] {
            let algo = match bytes {
                16 => "md5",
                20 => "sha1",
                32 => "sha256",
                _ => "sha512",
            };
            assert!(hash(&format!("{algo}:{}", "0".repeat(bytes * 2))));
            assert!(hash(&format!(
                "{algo}:{}",
                "0".repeat((bytes * 8).div_ceil(5))
            )));
            assert!(hash(&format!("{algo}-{}", STANDARD.encode(vec![0; bytes]))));
        }
        assert!(!hash(&format!("sha256:{}", "z".repeat(52))));
        assert!(!hash("sha256-invalid"));
    }

    #[test]
    fn standard_optional_fields_must_be_usable_by_default_nix() {
        for field in [
            "Compression: made-up\n",
            "Sig: invalid\n",
            "Sig: key:not-base64\n",
            "Sig: :AA==\n",
            "Sig: key:\n",
            "Sig: key:AA==\n",
            "CA: invalid\n",
            "CA: fixed:git:sha256:0000000000000000000000000000000000000000000000000000\n",
        ] {
            assert!(references(&(leaf() + field), PATH).is_err(), "{field:?}");
        }
        assert!(references(&leaf().replace("sha256:", "blake3:"), PATH).is_err());
        assert!(references(
            &(leaf() + &format!("FileHash: blake3:{}\n", "0".repeat(52))),
            PATH
        )
        .is_err());
        for compression in [
            "", "none", "identity", "br", "bzip2", "gzip", "xz", "zstd", "lz4", "lzip",
        ] {
            assert!(references(&(leaf() + &format!("Compression: {compression}\n")), PATH).is_ok());
        }
        for ca in [
            "".into(),
            format!("text:sha256:{}", "0".repeat(52)),
            format!("fixed:sha256:{}", "0".repeat(52)),
            format!("fixed:r:sha256:{}", "0".repeat(52)),
        ] {
            assert!(
                references(&(leaf() + &format!("CA: {ca}\n")), PATH).is_ok(),
                "{ca:?}"
            );
        }
        let signature = STANDARD.encode([0; 64]);
        assert!(references(
            &(leaf() + &format!("Sig: cache-a:{signature}\nSig: cache-b:{signature}\n")),
            PATH
        )
        .is_ok());
    }
}
