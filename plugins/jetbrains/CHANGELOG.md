# Changelog

All notable changes to the ASC plugin for JetBrains IDEs are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Signing settings page at **Settings > Version Control > asc Signing**. It shows the signing key's source, path, key ID or fingerprint, the format and whether the key is usable, plus the commit and tag signing switches. Apply writes only the changed fields through `asc config` to `user.signingkey`, `gpg.format`, `commit.gpgsign` and `tag.gpgsign`, and **Test signing** runs `asc signing test`. Key content is never shown or stored.
