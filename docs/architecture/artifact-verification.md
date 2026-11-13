# Independent Artifact Verification

## Trust Boundary

CI execution, artifact acceptance, code review and merge are separate gates.
An artifact reporting success is not accepted until independent validation
rechecks its raw reports and exact producing run. A code-review result is not
evidence that the reported media tests ran successfully.

Keep four locations distinct:

- The candidate checkout identifies the code under test. Bind repository ID,
  PR, base, head and tree to independently acquired operator scope.
- The control bundle supplies executable validation code. Select it from an
  already accepted release outside the candidate change, freeze its complete
  executable dependency manifest and pin that manifest's digest independently.
- The raw input directory contains acquired run/job/artifact records, original
  ZIPs and their binding manifest. Treat these as evidence, not executable code
  or authority to choose weaker validation policy.
- The derived output directory receives bounded extraction and the resulting
  proof. It must be fresh; failed attempts must not replace original evidence
  or overwrite an accepted proof.

Record producer-source identity separately from validator identity. An operator
must not substitute one for the other. In particular, the media helper imports
the adjacent UI helper: pin both and every executable wrapper/core dependency.
Do not resolve validator modules through the candidate checkout, its working
directory, ambient import paths or an undeclared bytecode cache.

## Verification Modes

Ordinary acceptance retains the unchanged-helper rule. Both producer helper
files must be outside the assigned base-to-head diff, and their worktree,
base-blob, head-blob and expected pinned bytes must agree.

Helper migration is a distinct operator-authorized scope. Bind the exact helper
path, old/new content identities and candidate range. A helper cannot authorize
its own migration through its artifact manifest. Continue validating the new
producer's reports using the independent old control bundle; never import the
new helper to decide whether its own artifacts pass. Unlisted helper changes
are rejected. Changing the report contract needs a separate design, not a
generic skip-check option.

Both modes retain the same native3 and macOS5/tvOS5 cases, all four task fixture
scenarios, complete run/job/page identities, artifact expiry/digest/size checks,
and the existing deadline, ZIP, extraction and diagnostic bounds. Missing,
skipped, malformed, unreadable or mismatched evidence must not become success.

Revalidate the control bundle and source identities before and after a run.
Protect object identity and content stability, rather than treating every
metadata timestamp change as mutation. Reject escapes, symlink substitution
and mixed releases. Execute the exact bytes whose digest was verified, including
the core and the media helper's immediate UI import. Reopening a pathname or
accepting cached bytecode after hashing is not equivalent; a later hash failure
cannot undo code that already executed. A failure must retain an explicit error
without accepted proof; it does not authorize an automatic retry or a weaker
check.

## First Migration

Deliver the new verification interface without changing the existing UI/media
helpers. Its own merge gate continues to use the prior accepted tools and their
ordinary helper rule. Unit tests may execute the new interface as the test
subject; they do not make that interface an independent acceptance control.

The old baseline was recovered from PR #101 and replayed once offline using
unchanged controls and independent input copies. The newly generated proof was
byte-identical to its accepted predecessor. This establishes the old baseline,
not acceptance of the new interface or any later feature branch.

After the interface passes complete local/remote acceptance, current-head
GitHub Codex review and conversation resolution, merge normally. Pin the merged
release commit and whole-bundle digest, then smoke-test that fixed release
before using it to gate a separate helper-migration PR.

After the helper-migration PR passes independent old-control acceptance and
merges, pin a successor control bundle from that accepted release. Preserve
the accepted verifier implementation and bind the now-accepted helper bytes;
the old A helper hash cannot serve as the ordinary-mode pin for a later source
whose base and head both contain the B helper. Keep the A-to-B trust transition
and both manifests as evidence. Packaging or smoke tests are not a replacement
for B's prior independent acceptance.

Only after that helper fix lands should dependent feature PRs integrate updated
master and repeat acceptance against their genuine new scope. Do not shorten
BASE, drop the old guard or reuse old-head evidence to manufacture a pass.

## Acceptance Limits

Local native/operator tests retain the pinned signing profile and stable signed
executable paths. Set and verify system mute before playback and keep it muted
afterward. Remote artifact validation is offline and must not invoke media/UI
runner entrypoints, access credentials or make network requests.

Neither muted execution nor report inspection establishes audible playback,
visible black-screen behavior, authenticated live Bilibili playback or physical
Apple TV acceptance. Faster-upshift claims still require their separate complete
OFF/ON playback comparison.

## Operator Interface

Run the fixed entrypoint from the independently pinned control release, not the
copy in the candidate checkout. Use an external bounded supervisor for the same
300-second artifact-verification deadline and retained-output cap as the prior
acceptance procedure:

```sh
python3 -I -S /trusted/read-only-bundle/verify_ci_artifacts.py \
  --scope /operator/frozen-scope.json \
  --checkout /candidate/checkout \
  --control-root /trusted/read-only-bundle \
  --control-release RELEASE_ID \
  --control-manifest-sha256 MANIFEST_SHA256 \
  --input-root /evidence/immutable-input \
  --output-root /evidence/fresh-derived-output
```

The operator scope is a closed JSON document binding repository/PR/base/head/tree,
the exact run/job/page/artifact records, producer helper hashes and route. Keep
it outside candidate control and freeze its identity independently. The input
evidence manifest retains its existing schema and must match the assigned scope;
it cannot supply a different scope or choose migration mode.

`route` is `ordinary` or `helper-migration`. Ordinary scope requires `migration`
to be `null`. Migration scope identifies exactly one existing helper through
`path`, `from_sha256`, `to_sha256` and `report_contract`; the supported unchanged
contract is `native3-ui5-fixture4`. `producer_helpers` pins both actual producer
helper files at the assigned head. There is no broad allow-changed-helper flag.

The control manifest has only `schema_version`, `release_id` and `files`, with
the complete fixed executable file set and SHA-256 values. Independently pin its
whole-file digest and release ID. Undeclared files, bytecode caches, symlinks,
mixed versions and writable control objects are rejected. Materialize a minimal
bundle rather than pointing at a general working directory. Operator-created
scope, bundle and raw input files must not contain cookies, keys or credentials.

The flat bundle contains `control-manifest.json` plus exactly these files,
materialized from `scripts/` in the selected accepted release:

- `verify_ci_artifacts.py`
- `artifact_validation.py`
- `media-network-e2e.py`
- `ui-e2e.py`

Use `schema_version: 1`, the independently selected release identity and a
`files` map from those four basenames to their SHA-256 values. Pin the exact
serialized manifest bytes, not a later reformatted equivalent. Make the bundle
and its files read-only before execution. No credentials, extra modules,
symlinks or cache files belong in this directory.

The reusable Python API is `artifact_validation.validate_all` with keyword-only
`scope_path`, `checkout`, `control_root`, `control_release_id`,
`control_manifest_sha256`, `input_root` and `output_root`. This API has the same
trust boundary as the command line; importing it from a candidate does not make
it an independently released acceptance control.

## Synthetic Git Fixtures

Artifact unit tests create disposable Git repositories without borrowing the
operator's signing identity. Their Git invocations disable commit signing only
for synthetic fixture commands, including when inherited global configuration
requires signing and the test environment has no private key. A hostile-config
regression checks that the GPG executable is never called and that inherited
configuration is not modified. Real project commits retain their required GPG
signatures; this is separate from fixed Apple signing for native media tests.
