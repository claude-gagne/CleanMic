# Security Policy

CleanMic takes security seriously. If you believe you have found a
vulnerability in the CleanMic source or in the official AppImage, please
report it privately rather than opening a public report.

## Reporting a Vulnerability

Please use GitHub's private vulnerability reporting for this repository:

**https://github.com/claude-gagne/CleanMic/security/advisories/new**

See GitHub's documentation on
[privately reporting a security vulnerability](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability)
for details on the flow.

Email reports are not accepted.

## Scope

In scope:

- The CleanMic source code in this repository.
- The official AppImage published at
  https://github.com/claude-gagne/CleanMic/releases.

## Out of Scope

- **Khip engine binaries or models.** The Khip engine is an optional,
  user-supplied library; CleanMic does not distribute it.
  Vulnerabilities in Khip itself should be reported to its upstream
  maintainer, not here.
- **System dependencies** - PipeWire, GTK4, libadwaita, the Linux kernel,
  D-Bus, or LADSPA plugins from other projects. Please report those to
  their respective upstream projects.
- **User-supplied LADSPA plugins** substituted for the bundled
  `libdeep_filter_ladspa.so`.

## What to Expect

CleanMic is solo-maintained. Expect an acknowledgment within about two
weeks of a report. Fix timelines vary with severity and with available
maintainer time; no fixed disclosure window is promised.
