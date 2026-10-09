// SPDX-License-Identifier: GPL-2.0-only
// Adapted from VIPTV core native_metainfo; generic validation carries no product identity.
//! Bounded, generic v1 metainfo validation before engine allocation or file access.
//! Byte slices stay private; errors never contain dictionary keys or paths.
use sha1::{Digest, Sha1};
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

type Result<T> = std::result::Result<T, InvalidMetainfo>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMetainfo;
impl std::fmt::Display for InvalidMetainfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid native metainfo")
    }
}
impl std::error::Error for InvalidMetainfo {}
pub const MAX_METAINFO_BYTES: usize = 4_194_304;
enum Value<'a> {
    Bytes(&'a [u8]),
    Integer(u64),
    List(Vec<Value<'a>>),
    Dictionary(Vec<(&'a [u8], Value<'a>)>),
}
struct Parser<'a> {
    bytes: &'a [u8],
    offset: usize,
    values: usize,
}
fn invalid() -> InvalidMetainfo {
    InvalidMetainfo
}
impl<'a> Parser<'a> {
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let start = self.offset;
        while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
            self.offset += 1;
        }
        let token = self.bytes.get(start..self.offset).ok_or_else(invalid)?;
        if token.is_empty()
            || (token.len() > 1 && token[0] == b'0')
            || self.bytes.get(self.offset) != Some(&b':')
        {
            return Err(invalid());
        }
        let size: usize = std::str::from_utf8(token)
            .map_err(|_| invalid())?
            .parse()
            .map_err(|_| invalid())?;
        if size > 4_194_304 {
            return Err(invalid());
        }
        self.offset += 1;
        let end = self.offset.checked_add(size).ok_or_else(invalid)?;
        let data = self.bytes.get(self.offset..end).ok_or_else(invalid)?;
        self.offset = end;
        Ok(data)
    }
    /// Allocation-free structural pass before constructing bounded containers.
    fn scan(&mut self, depth: usize, file_list: bool) -> Result<()> {
        self.values += 1;
        if depth > 32 || self.values > 65_536 {
            return Err(invalid());
        }
        match self.bytes.get(self.offset).copied() {
            Some(b'0'..=b'9') => {
                self.bytes()?;
            }
            Some(b'i') => {
                self.offset += 1;
                let start = self.offset;
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                }
                let token = &self.bytes[start..self.offset];
                if token.is_empty()
                    || (token.len() > 1 && token[0] == b'0')
                    || self.bytes.get(self.offset) != Some(&b'e')
                {
                    return Err(invalid());
                }
                std::str::from_utf8(token)
                    .map_err(|_| invalid())?
                    .parse::<u64>()
                    .map_err(|_| invalid())?;
                self.offset += 1;
            }
            Some(b'l') => {
                self.offset += 1;
                let mut count = 0;
                while self.bytes.get(self.offset) != Some(&b'e') {
                    count += 1;
                    if file_list && count > 4096 {
                        return Err(invalid());
                    }
                    self.scan(depth + 1, false)?;
                }
                self.offset += 1;
            }
            Some(b'd') => {
                self.offset += 1;
                let mut previous: Option<&[u8]> = None;
                while self.bytes.get(self.offset) != Some(&b'e') {
                    self.values += 1;
                    if self.values > 65_536 {
                        return Err(invalid());
                    }
                    let key = self.bytes()?;
                    if previous.is_some_and(|p| p >= key) {
                        return Err(invalid());
                    }
                    previous = Some(key);
                    self.scan(depth + 1, key == b"files")?;
                }
                self.offset += 1;
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    fn value(&mut self, depth: usize) -> Result<Value<'a>> {
        self.values += 1;
        if depth > 32 || self.values > 65_536 {
            return Err(invalid());
        }
        match self.bytes.get(self.offset).copied() {
            Some(b'0'..=b'9') => self.bytes().map(Value::Bytes),
            Some(b'i') => {
                self.offset += 1;
                let start = self.offset;
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                }
                let token = &self.bytes[start..self.offset];
                if token.is_empty()
                    || (token.len() > 1 && token[0] == b'0')
                    || self.bytes.get(self.offset) != Some(&b'e')
                {
                    return Err(invalid());
                }
                self.offset += 1;
                Ok(Value::Integer(
                    std::str::from_utf8(token)
                        .map_err(|_| invalid())?
                        .parse()
                        .map_err(|_| invalid())?,
                ))
            }
            Some(b'l') => {
                self.offset += 1;
                let mut list = Vec::new();
                while self.bytes.get(self.offset) != Some(&b'e') {
                    list.push(self.value(depth + 1)?);
                }
                self.offset += 1;
                Ok(Value::List(list))
            }
            Some(b'd') => {
                self.offset += 1;
                let mut map = Vec::new();
                let mut previous: Option<&[u8]> = None;
                while self.bytes.get(self.offset) != Some(&b'e') {
                    self.values += 1;
                    if self.values > 65_536 {
                        return Err(invalid());
                    }
                    let key = self.bytes()?;
                    if previous.is_some_and(|p| p >= key) {
                        return Err(invalid());
                    }
                    previous = Some(key);
                    map.push((key, self.value(depth + 1)?));
                }
                self.offset += 1;
                Ok(Value::Dictionary(map))
            }
            _ => Err(invalid()),
        }
    }
}
fn dictionary<'a>(v: &'a Value<'a>) -> Result<&'a [(&'a [u8], Value<'a>)]> {
    match v {
        Value::Dictionary(m) => Ok(m),
        _ => Err(invalid()),
    }
}
fn field<'a>(map: &'a [(&'a [u8], Value<'a>)], key: &[u8]) -> Result<&'a Value<'a>> {
    map.iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .ok_or_else(invalid)
}
fn optional<'a>(map: &'a [(&'a [u8], Value<'a>)], key: &[u8]) -> Option<&'a Value<'a>> {
    map.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}
fn integer(v: &Value<'_>) -> Result<u64> {
    match v {
        Value::Integer(n) => Ok(*n),
        _ => Err(invalid()),
    }
}
fn bytes<'a>(v: &'a Value<'a>) -> Result<&'a [u8]> {
    match v {
        Value::Bytes(b) => Ok(b),
        _ => Err(invalid()),
    }
}
fn component(v: &Value<'_>) -> Result<String> {
    let s = std::str::from_utf8(bytes(v)?).map_err(|_| invalid())?;
    if s.is_empty()
        || matches!(s, "." | "..")
        || s.ends_with('.')
        || s.ends_with(' ')
        || s.chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(invalid());
    }
    Ok(s.nfc().collect())
}
fn path(v: &Value<'_>) -> Result<Vec<String>> {
    match v {
        Value::List(list) if !list.is_empty() => list.iter().map(component).collect(),
        _ => Err(invalid()),
    }
}
fn equivalent_component(map: &[(&[u8], Value<'_>)], key: &[u8], utf8: &[u8]) -> Result<String> {
    let name = component(field(map, key)?)?;
    if optional(map, utf8)
        .map(component)
        .transpose()?
        .is_some_and(|s| s != name)
    {
        return Err(invalid());
    }
    Ok(name)
}
fn inspect_info(info: &Value<'_>) -> Result<Vec<u64>> {
    let map = dictionary(info)?;
    let keys: &[&[u8]] = &[
        b"name",
        b"name.utf-8",
        b"piece length",
        b"pieces",
        b"length",
        b"files",
    ];
    if map.iter().any(|(k, _)| !keys.contains(k)) {
        return Err(invalid());
    }
    equivalent_component(map, b"name", b"name.utf-8")?;
    let piece_length = integer(field(map, b"piece length")?)?;
    if !(16_384..=16_777_216).contains(&piece_length) || !piece_length.is_power_of_two() {
        return Err(invalid());
    }
    let pieces = bytes(field(map, b"pieces")?)?;
    let mut lengths = Vec::new();
    match (optional(map, b"length"), optional(map, b"files")) {
        (Some(length), None) => lengths.push(integer(length)?),
        (None, Some(Value::List(files))) if !files.is_empty() && files.len() <= 4096 => {
            let mut paths: BTreeSet<Vec<String>> = BTreeSet::new();
            for file in files {
                let file = dictionary(file)?;
                if file
                    .iter()
                    .any(|(k, _)| ![b"length".as_slice(), b"path", b"path.utf-8"].contains(k))
                {
                    return Err(invalid());
                }
                let name = path(field(file, b"path")?)?;
                let collision_name: Vec<String> = name
                    .iter()
                    .map(|part| part.to_lowercase().nfc().collect())
                    .collect();
                if optional(file, b"path.utf-8")
                    .map(path)
                    .transpose()?
                    .is_some_and(|p| p != name)
                    || paths
                        .iter()
                        .any(|p| p.starts_with(&collision_name) || collision_name.starts_with(p))
                {
                    return Err(invalid());
                }
                paths.insert(collision_name);
                lengths.push(integer(field(file, b"length")?)?);
            }
        }
        _ => return Err(invalid()),
    }
    if lengths.contains(&0) {
        return Err(invalid());
    }
    let total = lengths
        .iter()
        .try_fold(0u64, |sum, n| sum.checked_add(*n))
        .ok_or_else(invalid)?;
    let count = total.checked_add(piece_length - 1).ok_or_else(invalid)? / piece_length;
    if count.checked_mul(20) != Some(pieces.len() as u64) {
        return Err(invalid());
    }
    Ok(lengths)
}

fn preflight(data: &[u8], depth: usize) -> Result<()> {
    if data.is_empty() || data.len() > MAX_METAINFO_BYTES {
        return Err(invalid());
    }
    let mut parser = Parser {
        bytes: data,
        offset: 0,
        values: 0,
    };
    parser.scan(depth, false)?;
    if parser.offset != data.len() {
        return Err(invalid());
    }
    Ok(())
}
fn tracker(v: &Value<'_>) -> Result<()> {
    let value = std::str::from_utf8(bytes(v)?).map_err(|_| invalid())?;
    let u = url::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(u.scheme(), "http" | "https" | "udp")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}
/// Exact caller-verified authorization facts; this type has no wire serialization.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NativeSelection {
    pub info_hash: [u8; 20],
    pub file_index: u32,
    pub verified_size: Option<u64>,
}
impl std::fmt::Debug for NativeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeSelection(<redacted>)")
    }
}
impl NativeSelection {
    pub fn from_hex(
        hash: &str,
        file_index: u32,
        verified_size: impl Into<Option<u64>>,
    ) -> Result<Self> {
        let verified_size = verified_size.into();
        if hash.len() != 40
            || file_index >= 4096
            || verified_size == Some(0)
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
        let mut info_hash = [0; 20];
        hex::decode_to_slice(hash, &mut info_hash).map_err(|_| invalid())?;
        Ok(Self {
            info_hash,
            file_index,
            verified_size,
        })
    }
    pub fn verify(&self, metadata: &NativeMetainfo) -> Result<()> {
        let selected = metadata.file_sizes().get(self.file_index as usize).copied();
        if self.info_hash != metadata.info_hash()
            || selected.is_none_or(|size| size == 0)
            || self
                .verified_size
                .is_some_and(|size| selected != Some(size))
        {
            return Err(invalid());
        }
        Ok(())
    }
}
/// Vetted metadata contains no presentation paths or tracker annotations.
pub struct NativeMetainfo {
    canonical: Vec<u8>,
    hash: [u8; 20],
    file_sizes: Vec<u64>,
    trackers: Vec<String>,
}
impl std::fmt::Debug for NativeMetainfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeMetainfo(<redacted>)")
    }
}
impl NativeMetainfo {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    pub fn info_hash(&self) -> [u8; 20] {
        self.hash
    }
    pub fn info_hash_hex(&self) -> String {
        hex::encode(self.hash)
    }
    pub fn file_sizes(&self) -> &[u64] {
        &self.file_sizes
    }
    pub fn trackers(&self) -> &[String] {
        &self.trackers
    }
    pub fn total_payload_bytes(&self) -> u64 {
        self.file_sizes.iter().sum()
    }
    pub fn verify_selection(
        &self,
        hash: Option<&str>,
        index: u32,
        expected_size: Option<u64>,
    ) -> Result<()> {
        if hash.is_some_and(|h| {
            h.len() != 40
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || h != self.info_hash_hex()
        }) {
            return Err(invalid());
        }
        let selected = *self.file_sizes.get(index as usize).ok_or_else(invalid)?;
        if selected == 0 || expected_size.is_some_and(|n| n != selected) {
            return Err(invalid());
        }
        Ok(())
    }
}
/// Inspect peer-discovered exact info bytes before typed metadata or storage allocation.
pub fn validate_native_info(data: &[u8]) -> Result<Vec<u64>> {
    preflight(data, 1)?;
    let mut parser = Parser {
        bytes: data,
        offset: 0,
        values: 0,
    };
    let info = parser.value(1)?;
    inspect_info(&info)
}
/// Strip syntactically vetted outer fields while preserving every exact info byte.
pub fn vet_native_metainfo(data: &[u8]) -> Result<NativeMetainfo> {
    preflight(data, 0)?;
    let mut parser = Parser {
        bytes: data,
        offset: 1,
        values: 1,
    };
    if data.first() != Some(&b'd') {
        return Err(invalid());
    }
    let mut info_range = None;
    let mut sizes = None;
    let mut trackers = Vec::new();
    while parser.bytes.get(parser.offset) != Some(&b'e') {
        let key = parser.bytes()?;
        let start = parser.offset;
        let value = parser.value(1)?;
        match key {
            b"info" => {
                sizes = Some(inspect_info(&value)?);
                info_range = Some(start..parser.offset);
            }
            b"announce" => {
                tracker(&value)?;
                let value = std::str::from_utf8(bytes(&value)?)
                    .map_err(|_| invalid())?
                    .to_owned();
                if !trackers.contains(&value) {
                    trackers.push(value)
                }
            }
            b"announce-list" => match &value {
                Value::List(tiers) => {
                    for tier in tiers {
                        match tier {
                            Value::List(urls) => {
                                for u in urls {
                                    tracker(u)?;
                                    let value = std::str::from_utf8(bytes(u)?)
                                        .map_err(|_| invalid())?
                                        .to_owned();
                                    if !trackers.contains(&value) {
                                        trackers.push(value)
                                    }
                                }
                            }
                            _ => return Err(invalid()),
                        }
                    }
                }
                _ => return Err(invalid()),
            },
            b"creation date" => {
                integer(&value)?;
            }
            b"comment" | b"created by" | b"encoding" => {
                bytes(&value)?;
            }
            _ => return Err(invalid()),
        }
    }
    let info = &data[info_range.ok_or_else(invalid)?];
    if trackers.len() > 32 {
        return Err(invalid());
    }
    let mut sha = Sha1::new();
    sha.update(info);
    let mut canonical = Vec::with_capacity(info.len() + 8);
    canonical.extend_from_slice(b"d4:info");
    canonical.extend_from_slice(info);
    canonical.push(b'e');
    if canonical.len() > MAX_METAINFO_BYTES {
        return Err(invalid());
    }
    Ok(NativeMetainfo {
        canonical,
        hash: sha.finalize().into(),
        file_sizes: sizes.ok_or_else(invalid)?,
        trackers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn string(value: &[u8]) -> Vec<u8> {
        let mut b = format!("{}:", value.len()).into_bytes();
        b.extend_from_slice(value);
        b
    }
    fn dict(fields: Vec<(&[u8], Vec<u8>)>) -> Vec<u8> {
        let mut fields = fields;
        fields.sort_by(|a, b| a.0.cmp(b.0));
        let mut b = vec![b'd'];
        for (key, v) in fields {
            b.extend(string(key));
            b.extend(v);
        }
        b.push(b'e');
        b
    }
    fn single_info() -> Vec<u8> {
        dict(vec![
            (b"length", b"i1e".to_vec()),
            (b"name", string(b"owned.mp4")),
            (b"piece length", b"i16384e".to_vec()),
            (b"pieces", string(&[7; 20])),
        ])
    }
    fn multi_info(paths: &[Vec<&str>]) -> Vec<u8> {
        let mut files = vec![b'l'];
        for p in paths {
            let mut path = vec![b'l'];
            for c in p {
                path.extend(string(c.as_bytes()));
            }
            path.push(b'e');
            files.extend(dict(vec![(b"length", b"i1e".to_vec()), (b"path", path)]));
        }
        files.push(b'e');
        dict(vec![
            (b"files", files),
            (b"name", string(b"owned")),
            (b"piece length", b"i16384e".to_vec()),
            (b"pieces", string(&[7; 20])),
        ])
    }
    #[test]
    fn preserve_exact_info_hash_strip_vetted_annotations_and_verify_selection() {
        let info = single_info();
        let bare = dict(vec![(b"info", info.clone())]);
        let annotated = dict(vec![
            (b"info", info),
            (b"announce", string(b"udp://tracker.invalid:6969")),
            (b"comment", string(b"annotation")),
        ]);
        let vetted = vet_native_metainfo(&annotated).unwrap();
        assert_eq!(vetted.canonical_bytes(), bare);
        assert_eq!(
            vetted.trackers(),
            &["udp://tracker.invalid:6969".to_string()]
        );
        assert_eq!(
            vetted.info_hash(),
            vet_native_metainfo(&bare).unwrap().info_hash()
        );
        let hash = vetted.info_hash_hex();
        assert!(vetted.verify_selection(Some(&hash), 0, Some(1)).is_ok());
        assert!(vetted
            .verify_selection(Some("0000000000000000000000000000000000000000"), 0, Some(1))
            .is_err());
        assert!(vetted.verify_selection(Some(&hash), 1, Some(1)).is_err());
        assert!(vetted.verify_selection(Some(&hash), 0, Some(2)).is_err());
        assert_eq!(format!("{vetted:?}"), "NativeMetainfo(<redacted>)");
    }
    #[test]
    fn reject_outer_hints_unknown_fields_and_tracker_credentials() {
        for key in [
            b"url-list".as_slice(),
            b"httpseeds",
            b"nodes",
            b"peers",
            b"unknown",
        ] {
            assert!(vet_native_metainfo(&dict(vec![
                (b"info", single_info()),
                (key, string(b"denied"))
            ]))
            .is_err());
        }
        for u in [
            b"ftp://tracker.invalid".as_slice(),
            b"https://user:secret@tracker.invalid/announce",
            b"udp://tracker.invalid:6969/#secret",
        ] {
            assert!(vet_native_metainfo(&dict(vec![
                (b"info", single_info()),
                (b"announce", string(u))
            ]))
            .is_err());
        }
    }
    #[test]
    fn reject_noncanonical_trailing_malformed_and_bounded_structures() {
        for b in [
            b"d1:ai01ee".as_slice(),
            b"d1:ai-0ee",
            b"d1:ai-1ee",
            b"d1:ai0e1:ai0ee",
            b"d1:bi0e1:ai0ee",
            b"d1:a99999999999999999999999999999999999:x",
            b"d1:ai18446744073709551616ee",
            b"d1:ai0eejunk",
        ] {
            assert!(preflight(b, 0).is_err());
        }
        let mut deep = vec![b'l'; 34];
        deep.extend(vec![b'e'; 34]);
        assert!(preflight(&deep, 0).is_err());
        let mut values = vec![b'l'];
        for _ in 0..65_536 {
            values.extend_from_slice(b"i0e");
        }
        values.push(b'e');
        assert!(preflight(&values, 0).is_err());
        assert!(preflight(&vec![b'0'; MAX_METAINFO_BYTES + 1], 0).is_err());
        let paths = vec![vec!["x"]; 4097];
        assert!(preflight(&multi_info(&paths), 1).is_err());
    }
    #[test]
    fn reject_paths_collisions_overlap_and_unsupported_info() {
        for paths in [
            vec![vec![".."]],
            vec![vec!["a/b"]],
            vec![vec!["a\\b"]],
            vec![vec!["C:x"]],
            vec![vec!["x."]],
            vec![vec!["x "]],
            vec![vec!["x"], vec!["X"]],
            vec![vec!["x"], vec!["x", "y"]],
            vec![vec!["é"], vec!["é"]],
        ] {
            assert!(validate_native_info(&multi_info(&paths)).is_err());
        }
        for key in [
            b"private".as_slice(),
            b"meta version",
            b"file tree",
            b"attr",
            b"symlink path",
        ] {
            let mut info = single_info();
            info.pop();
            info.extend(string(key));
            info.extend_from_slice(b"i0ee");
            assert!(validate_native_info(&info).is_err());
        }
        let mut info = single_info();
        let from = b"i16384e";
        let pos = info.windows(from.len()).position(|s| s == from).unwrap();
        info.splice(pos..pos + from.len(), b"i16385e".iter().copied());
        assert!(validate_native_info(&info).is_err());
    }

    #[test]
    fn optional_verified_size_never_changes_exact_hash_index_or_positive_length() {
        let vetted = vet_native_metainfo(&dict(vec![(b"info", single_info())])).unwrap();
        let hash = vetted.info_hash_hex();
        let unknown = NativeSelection::from_hex(&hash, 0, None).unwrap();
        assert!(unknown.verify(&vetted).is_ok());
        let size = vetted.file_sizes()[0];
        assert!(NativeSelection::from_hex(&hash, 0, size)
            .unwrap()
            .verify(&vetted)
            .is_ok());
        assert!(NativeSelection::from_hex(&hash, 0, size + 1)
            .unwrap()
            .verify(&vetted)
            .is_err());
        assert!(NativeSelection::from_hex(&hash, 1, None)
            .unwrap()
            .verify(&vetted)
            .is_err());
        assert!(NativeSelection::from_hex(&"0".repeat(40), 0, None)
            .unwrap()
            .verify(&vetted)
            .is_err());
        assert!(NativeSelection::from_hex(&hash, 0, Some(0)).is_err());
        let empty = NativeMetainfo {
            canonical: vec![],
            hash: unknown.info_hash,
            file_sizes: vec![0],
            trackers: vec![],
        };
        assert!(unknown.verify(&empty).is_err());
        assert!(empty.verify_selection(Some(&hash), 0, None).is_err());
    }
}
