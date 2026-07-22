# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Automated Linux release builds via GitHub Actions: pushing a `v*` tag builds
  `.deb` and `.AppImage` installers with `tauri-apps/tauri-action` and attaches
  them to a draft GitHub Release. Closes #1.
- Internationalization (i18n) with `react-i18next`: English is the default UI
  language, with a Portuguese (`pt-BR`) locale included. UI strings are
  organized into per-view namespaces.
- Open-source project scaffolding: `LICENSE` (MIT), `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, `SECURITY.md`, `AGENTS.md`, issue/PR templates.

### Changed

- The entire codebase (comments, log/error messages, docs) was translated from
  Portuguese to English ahead of the open-source release. Golden STT test
  fixtures remain in PT-BR by design (they are paired with Portuguese audio).
