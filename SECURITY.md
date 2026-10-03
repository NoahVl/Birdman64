# Security Policy

## Supported versions

Only the latest release of Birdman64 receives security fixes. Please update
before reporting a problem with an older version.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's **private vulnerability
reporting**: on this repository, open the **Security** tab and choose
**Report a vulnerability**. Do not open a public issue for a security
problem.

Please include what you can of: the Birdman64 version, your OS, steps or a
description of the affected code path, and your assessment of the impact.

## What is in scope

- The first-run downloader and extractor
  ([crates/pw64-cbuild fetch.rs](crates/pw64-cbuild/src/fetch.rs)): archive
  entry handling, path traversal checks, hash verification, the network fetch.
- The zip ROM loader ([crates/pw64-rom](crates/pw64-rom)): a hostile
  zip-wrapped ROM image must not be able to escape its reads.
- Texture packs (`PW64_TEX_PACKS`): PNG decoding and pack path handling.
- Anything else that lets a malicious file, URL or asset read, write or
  execute outside what the game is supposed to touch.

## Please do not include ROMs

**Never attach or link the game ROM** in a report (public or private): we
cannot distribute it, and reports are processed by volunteers. A SHA-1 hash
of the ROM is fine if it matters. Include a debugger trace, log excerpt or
synthetic input file instead of ROM content.
