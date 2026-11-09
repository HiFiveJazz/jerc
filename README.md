<div align="center">
  <img src="assets/images/logo.svg" alt="Jerc logo" width="500" />
  <h1>Jerc</h1>
  <p><strong>A keyboard-driven email client for the terminal, written in Rust.</strong></p>
  <p>
    <a href="#features">Features</a> ·
    <a href="#getting-started">Getting started</a> ·
    <a href="docs/keybindings.md">Keybindings</a> ·
    <a href="docs/configuration.md">Configuration</a> ·
    <a href="CONTRIBUTING.md">Contributing</a>
  </p>
</div>

---

Jerc is a batteries-included terminal mail app! Features include contacts, and composition together in a responsive terminal interface. Navigate with Vim-style keys, manage multiple accounts, and read rich messages without leaving your terminal.

## Features

| Feature| Highlights |
| --- | --- |
| **Mail** | IMAP and Maildir accounts, multiple mailboxes, background refresh, and cached message lists. |
| **Keyboard-first** | Vim-style navigation, relative line numbers, smooth scrolling, searchable `?` help, and mouse support. |
| **Fast triage** | Visual selection, persistent marks, bulk actions, archive, spam handling, and in-mailbox filtering. |
| **Rich reading** | HTML and plain-text views, tables, links, attachments, Git patch highlighting, and optional inline images. |
| **Compose naturally** | Modal editing, reply/reply-all/forward, recipient suggestions, attachment picker, drafts, and send preview. |
| **Your setup** | Multiple accounts, built-in and custom themes, adjustable panels, and rounded or square borders. |

### Privacy by default

Jerc strives to be privacy first by limiting tracking requests. Jerc uses TLS for supported network transports, supports OAuth and credential commands, and keeps its native contacts database local (and soon encrypted). End-to-end email encryption are planned for future Jerc releases! 

See [configuration](docs/configuration.md) for the relevant controls.

## Getting started

**Requirements:** Rust 1.98 or newer and a terminal with modern Unicode support. Optional terminal graphics require a compatible terminal; PDF thumbnails use `pdftoppm` when installed.

```sh
cargo build --release
./target/release/jerc --version
```

For a local Maildir, you can try Jerc without configuring an online account:

```sh
./target/release/jerc --maildir ~/Mail
```

For IMAP, SMTP, or OAuth accounts, copy the sample configuration and edit it:

```sh
mkdir -p ~/.config/jerc
cp config.example.toml ~/.config/jerc/config.toml
./target/release/jerc
```

The example includes a sample Maildir account; replace or remove it for your own setup. Configuration follows your operating system's user configuration directory (the path above is typical on Linux).

> Jerc is currently built from source. Package-manager installation instructions will be added when packages are published.

## Essential keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move through messages |
| `J` / `K` | Change mailbox |
| `Enter` | Open message |
| `/` | Search/filter |
| `Space` | Mark message
| `v` / `V` | Visual selection |
| `a` / `d` | Archive / confirm deletion |
| `c` / `r` / `f` | Compose / reply / forward |
| `?` | Search all shortcuts for the current view |

The full reference lives in **[Keybindings](docs/keybindings.md)**. Press `?` inside Jerc for a searchable, context-aware shortcut palette.

## Documentation

- **[Keybindings](docs/keybindings.md)** — mail, reader, Compose, Contacts, attachments, and picker.
- **[Configuration](docs/configuration.md)** — accounts, privacy defaults, themes, layouts, and storage paths.
- **[OAuth & account discovery](docs/oauth.md)** — Google, Microsoft, and sign-in troubleshooting.
- **[Contacts](docs/contacts.md)** — contacts management and recipient autocomplete.
- **[Architecture](ARCHITECTURE.md)** — rendering, backend workers, state, and performance invariants.
- **[Contributing](CONTRIBUTING.md)** — development workflow and tests.

## Project status

Jerc is under active development. IMAP and Maildir reading, SMTP sending, drafts, and the UI are implemented; more advanced features such as full threading and IMAP IDLE are still planned. Expect changes before 1.0.

Released under the [MIT License](LICENSE).
