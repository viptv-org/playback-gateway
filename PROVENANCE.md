# Extraction provenance

The engine was extracted from `viptv-org/backend`, commit
`5db5000eefa6833d5ed1ab47780cb8aba1919438`, directory
`server/playback-engine`. Original source and qualification tests are retained.
The GPL-2.0-only license and original attribution continue to apply.

Subsequent changes remove product-specific naming, separate the public service
contract and remove the arbitrary 1080p delivery clamp. This extraction does not
by itself change the deployed VIPTV backend or prove gateway cutover complete.
