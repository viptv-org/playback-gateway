# Generic native torrent policy

This runtime-free crate validates bounded v1 metainfo and applies generic native
peer destination rules. It carries no application/account/profile identity.
Backend consumers may pin a source snapshot of this directory without rqbit,
media extraction or the engine; preserve source notices and the owning immutable
revision/checksum. Native dependency admission reuses this exact validator.

`metainfo::vet_native_metainfo` returns a redacted `NativeMetainfo` with canonical
info-only bytes, exact info hash, ordered file lengths and checked total payload.
Call `verify_selection` before emitting exact-file authorization. Transport
admission requires a `NativeSelection` with hash, index and positive verified size.
Errors contain no keys, paths, tracker URLs or input bytes.

`network::NetworkPolicy::PublicDhtTcpV1` filters every destination and entire DNS
answer sets. Its shipped bootstrap version/inventory are public constants.
`test-network-policy` is an isolated, non-default build feature for exact owned
private TCP endpoints; it permits no DHT and has no production wire selector.

Run `cargo test --locked -p torrent-policy` from the owning workspace. See
[network policy](../docs/NATIVE_NETWORK_POLICY.md) and
[provenance](../PROVENANCE.md) for bounds, source attribution and qualification.
