# Contributing

Thanks for your interest in Lightify. It's a one-person project, so this page keeps
things simple.

## Bugs and ideas

Open an [issue](https://github.com/JunkfoodJon/Lightify/issues/new/choose) and pick the
bug report or feature request form. For anything security-related, please follow
[SECURITY.md](SECURITY.md) instead of opening a public issue.

## Pull requests

Small, focused pull requests are welcome. For anything larger, please open an issue
first so we can agree on the approach before you spend time on it.

Before you open one:

- build with `cargo build --release --locked` from `lightify-shell/`, and run
  `cargo test` in both `lightify-shell/` and `lightify-core/`;
- if you changed the UI, render it with `Lightify.exe --shot out.png` and include the
  picture in the pull request;
- keep the app light: no new background polling, no browser engine, and no new
  dependency without a good reason;
- match the style of the code around your change.

## License of contributions

Lightify is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE.md). By
opening a pull request you agree that your contribution is released under the same
license, and that the maintainer may include it in Lightify under that license.
