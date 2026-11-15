# Contributing

Thank you for your interest. The project is at an early stage, so please open an issue to
discuss a large change before you start on it.

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Please report unacceptable
behavior to opensource@ameleo.fr.

## Developer Certificate of Origin

Every commit must be signed off, to certify that you wrote it or have the right to submit it
under the project's license ([Developer Certificate of Origin](https://developercertificate.org/)):

    git commit -s

This adds a `Signed-off-by: Your Name <you@example.com>` line with your real name. A check on
every pull request rejects commits without it. No contributor license agreement is needed.

## License of contributions

The agent is distributed under the GNU General Public License, version 3 only, and
contributions are accepted under the same license. By signing off a commit, you certify the
Developer Certificate of Origin for it and license it under the GPL, version 3 only.

New source files start with this line:

```rust
// SPDX-License-Identifier: GPL-3.0-only
```

Changes to a D-Bus interface go to
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) first.

## Commits and pull requests

- Write code comments, documentation, commit messages and pull request descriptions in plain
  English, with full sentences.
- Keep each pull request to one topic. Describe why the change is needed, what it does, and how
  it was tested.
- Keep `cargo fmt`, `cargo clippy` and `cargo test` clean on Windows; continuous integration runs
  them.

Commit messages follow the usual Git conventions:

- A subject line of about 50 characters, in the imperative mood ("Add a camera control",
  "Fix the file list size"), without a trailing period.
- A blank line, then a body wrapped at 72 characters that explains why the change is needed and
  what it does.

```
Retry the clipboard when another app holds it

OpenClipboard fails while another app has the clipboard open, and the
agent then dropped the host's data. Retry for up to 500 ms before
giving up, as Explorer does.

Signed-off-by: Your Name <you@example.com>
```

## AI-assisted contributions

You may use AI coding assistants, as in the Linux kernel and LLVM, under these conditions:

- A human reviews, understands and tests everything submitted, and answers for it in review.
- Substantial AI assistance is disclosed with an `Assisted-by:` trailer that names the assistant
  and model, and any tools it ran:

      Assisted-by: Claude:claude-opus-5-5
      Signed-off-by: Your Name <you@example.com>

- Only the human contributor signs off: an assistant cannot certify the Developer Certificate of
  Origin, and is not credited as a co-author.

Routine tools, such as editor completion of a few words, formatters and compilers, need no
mention.

## Reporting security issues

Do not open a public issue for a vulnerability; see [SECURITY.md](SECURITY.md).
