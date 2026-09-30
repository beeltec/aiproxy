# Agent instructions

## Release playbook

Versions follow [Semantic Versioning](https://semver.org/). The tag is `vX.Y.Z`. The version is only in `server/Cargo.toml` (and `server/Cargo.lock`).

1. Be on `main`, up to date with `origin/main`, with a clean tree and green CI (`gh run list --branch main`).
2. Select the next version from the commits since the last tag (`git log $(git describe --tags --abbrev=0)..HEAD --oneline`):
   - only `fix:` commits: patch (`0.1.0` → `0.1.1`)
   - a `feat:` commit: minor (`0.1.1` → `0.2.0`)
   - a breaking change (`!` or `BREAKING CHANGE`): major. Before `1.0.0`, use minor.
3. Update `CHANGELOG.md`:
   - Move the items from `[Unreleased]` to a new `## [X.Y.Z] - YYYY-MM-DD` section. Add missing items for user-visible changes, with a link to the pull request.
   - Use the groups `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed` and `Security`.
   - Update the compare links at the bottom of the file.
4. Set `version = "X.Y.Z"` in `server/Cargo.toml`. Then run `cargo check` in `server/` to update `Cargo.lock`.
5. Commit both files as `chore(release): vX.Y.Z` and push `main`.
6. Tag and push: `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`.
7. The `Release` workflow pushes the image `ghcr.io/beeltec/aiproxy` with the tags `X.Y.Z`, `X.Y` and `latest`. Wait until it is green (`gh run watch`).
8. Create the GitHub release from the changelog section, with a `docker pull ghcr.io/beeltec/aiproxy:X.Y.Z` line: `gh release create vX.Y.Z --title vX.Y.Z --notes-file <notes>`.
