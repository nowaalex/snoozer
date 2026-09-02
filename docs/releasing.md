# Release process

This page is for maintainers. It explains the one manual bootstrap and the automatic release path.

## Bootstrap `0.1.0`

Trusted Publishing cannot create a new crates.io package. First publish the already verified
`0.1.0` locally:

```sh
just ci
cargo publish
```

Then configure a trusted publisher for `snoozer` in crates.io. It must match this repository, the
`main` branch, the [`release-plz.yml`](../.github/workflows/release-plz.yml) workflow, and the
`crates-io` GitHub Environment. The workflow does not read a registry token.

## Later releases

1. Merge ordinary changes into `main`.
2. Release-plz creates or updates one Release PR with the version and `CHANGELOG.md` changes.
   It also runs the required CI check on that bot-created branch.
3. Review and merge that Release PR (use GitHub's normal **Create a merge commit** option).
4. The `Publish release` job reruns `just ci` and waits for approval in `crates-io`.
5. After approval, release-plz publishes to crates.io, creates `v<version>`, and creates the
   GitHub Release.

The release job runs only for a merged `release-plz-` branch because `release_always = false` is
owned by [`release-plz.toml`](../release-plz.toml). An ordinary merge cannot publish a crate.
The merge strategy matters: release-plz uses the release-PR commit to avoid picking up later,
unreviewed changes from `main`.

## Failure and retry

The release identity is the release-PR commit and its package version. crates.io, the Git tag, and
the GitHub Release are authoritative state. If GitHub or the registry connection fails after an
upload, rerun the same workflow from that commit: release-plz checks existing state before it
creates another release. Do not change the version to retry a possibly successful publish.

The workflow bounds registry publishing to ten minutes. A pending environment approval does not
start the job and therefore cannot publish by itself.
