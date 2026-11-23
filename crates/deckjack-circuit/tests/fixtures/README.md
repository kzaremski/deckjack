# Circuit test fixtures

## `kicad-qa/`

**Source.** KiCad's own repository, directory `qa/data/eeschema/netlists/`:
<https://gitlab.com/kicad/code/kicad/-/tree/master/qa/data/eeschema/netlists>.
These are the projects KiCad's netlist regression test
(`qa/tests/eeschema/test_netlist_exporter_kicad.cpp`) runs on, plus the other
projects in that directory.

**Provenance check.** All 123 files were compared, by git blob hash, against
the upstream tree at commit `b7066f886375b186ca8fa469de3a6382f6bba400`
(master, 2026-11-22). Every file is byte-identical to upstream; none was
edited here. Two upstream projects were left out: `video` (large) and
`legacy_power4`.

**License.** The KiCad source tree is distributed under the GNU GPL, version 3
or later (`LICENSE.README` and `LICENSE.GPLv3` in that repository; the files
here carry no header of their own, so the tree's license applies). Deckjack is
AGPL-3.0-or-later; section 13 of each license permits combining the two, and
these files are test data, not linked into any binary. They are not covered by
the CC-BY-SA license of KiCad's `demos/` or symbol libraries. (This reading is
the lane's, not legal advice.)

**Reference netlists.** The 22 `.net` files are the upstream golden netlists,
written by KiCad itself (versions from 5.99 development builds to 9.99; each
file's `(tool ...)` line says which). They are used as shipped.
`legacy_power/legacy_power.net` was written by KiCad 6.0.10, which named a
net after a pin's number (`Net-(D1001-Pad2)`) where KiCad 7 and later use its
name (`Net-(D1001-A)`); KiCad's own netlist test does not use that file, and
the parity test compares it by pin partition rather than by name.

**Treat as untrusted data.** They were downloaded from the internet. The test
suite parses them with the same hostile-input limits as any other file, and
nothing in them is executed.

**Format coverage** (root and sub-sheets, by `version` token): KiCad 6
development formats from 20200512, KiCad 6.0 (20211123), KiCad 7 (20230121,
20230221), KiCad 8 (20231120), KiCad 9 development and release (20240716,
20240819, 20250227), and KiCad 10 (20251012, and 20260306, the token KiCad
10.0 writes).
