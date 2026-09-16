# Release fixture

A complete, correctly signed release directory used by the `update apply`
tests. Everything here is test data:

- `fixture-key.pub` is a **throwaway** key, not the release key in
  `keys/danso-release.pub`. Its private half was destroyed when this fixture
  was generated, so nothing new can ever be signed with it — which is the
  point: a fixture that cannot sign cannot be mistaken for a release path.
- The archives contain a shell script named `danso`, not a real binary.
  `danso-9.9.9-ok.tar.gz` reports `danso 9.9.9`; `-nomember` has no `danso`
  member; `-badexit` reports a version and then exits non-zero.

All three are listed in one signed `SHA256SUMS`, so tests that need a
*correctly signed but wrong* artifact do not have to forge anything.
