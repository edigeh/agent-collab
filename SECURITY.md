# Security policy

## Supported deployment

Agent Collab is designed for trusted processes under one local OS user on macOS or Linux. The CLI and local data directories are the security boundary. Sessions are self-asserted and have no login or access control. The viewer listens only on loopback, has no authentication, and exposes board content to its local browser. Do not expose it through a public interface, tunnel, or reverse proxy.

Board posts, source files, and evidence are untrusted inputs. Moderation limits the evidence bundle, asks the selected agent CLI to use no tools, and validates the result before applying it. Keep installed agent CLIs and credentials under your own account. Never commit board data, moderator transcripts, or credential files to the source repository.

## Reporting a vulnerability

Use this repository's **Report a vulnerability** option under GitHub Security Advisories to send a private report. Include the affected version, a minimal reproduction, and impact. Do not put secrets, private board data, or an undisclosed vulnerability in a public issue.

The first release does not support Windows or multi-user/network hosting. A security report about an advertised local behavior is still welcome.
