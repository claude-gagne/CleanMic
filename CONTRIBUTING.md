# Contributing to CleanMic

CleanMic is a solo-maintained project by Claude Gagne. Bug reports and pull
requests are welcome, but response times can vary and not every contribution
will be merged. Keep expectations accordingly.

## Reporting bugs

Open a GitHub Issue. Please include your Linux distribution and version, your
PipeWire version (`pw-cli info 0 | head`), the engine you had selected, and
clear steps to reproduce.

## Security issues

Please do not open a public issue for security vulnerabilities. See
[`SECURITY.md`](SECURITY.md) for the private reporting process.

## Building from source

See [`README.md`](README.md) for system dependencies and the user-facing
build path. The developer shortcuts live in the `Makefile`:

- `make build` - release build with all features
- `make appimage` - build the AppImage bundle (see also `scripts/build-appimage.sh`)
- `make test` - run the unit tests
- `make fmt` - format sources with `cargo fmt`
- `make lint` - run `cargo clippy --all-features`

Please run `make fmt` and `make lint` before opening a pull request, and keep
each PR focused on a single concern. Large, unfocused PRs are likely to be
closed or asked to be split.

## Translations (i18n)

CleanMic ships a French translation, and all user-facing strings must stay
translatable. Two rules:

1. **Wrap user-facing strings** (menu labels, notifications, window text) in
   `gettext("…")`. Never build a visible label from a raw `format!("…")`.
2. **Add the French translation in the same change** — put the `msgstr` in
   `locale/fr/LC_MESSAGES/cleanmic.po`, then run `make mo`.

Install the local guard once after cloning:

```sh
bash scripts/install-git-hooks.sh
```

This installs a `pre-commit` hook (`scripts/pre-commit-i18n-check.sh`) that
blocks commits introducing an untranslated or unwrapped user-facing string.
The same check runs on pull requests as a backstop. In a genuine exception you
can bypass a single commit with `git commit --no-verify`, or mark a non-UI
literal with a trailing `// i18n-ignore` comment.

## Code of conduct

Participation in this project is governed by the
[Code of Conduct](CODE_OF_CONDUCT.md).
